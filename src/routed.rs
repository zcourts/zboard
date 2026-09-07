use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::model::{Agent, Message};
use crate::storage::{read_document, write_document};

const DOCUMENT_SUFFIX: &str = ".json.zst";
const CHECKPOINT_SCHEMA: &str = "aiboard.consumer.v1";
const EXPIRY_SCHEMA: &str = "aiboard.expiry.v1";
const MIGRATION_SCHEMA: &str = "aiboard.migration.v1";
const CACHE_LIMIT: usize = 4096;
const RECENT_MINUTES: i64 = 10;

#[derive(Default)]
pub struct RoutedState {
    pub agent_id: String,
    pub project: String,
    cursors: BTreeMap<String, RouteCursor>,
    loaded: HashSet<PathBuf>,
    warned: HashSet<PathBuf>,
    last_gc: Option<Instant>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Checkpoint {
    schema: String,
    id: String,
    agent: String,
    updated_at: String,
    routes: BTreeMap<String, RouteCursor>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct RouteCursor {
    high_water: String,
    #[serde(default)]
    overlap_ids: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Expiry {
    schema: String,
    message: String,
    expires_at: String,
    paths: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct MigrationReport {
    schema: String,
    migrated: usize,
    already_present: usize,
    retained_legacy: bool,
}

pub fn ensure(root: &Path) -> Result<PathBuf> {
    let v2 = root.join("v2");
    for path in [
        v2.join("messages/global"),
        v2.join("messages/projects"),
        v2.join("messages/groups"),
        v2.join("messages/direct"),
        v2.join("consumers"),
        v2.join("expiry"),
        v2.join("migrations"),
    ] {
        fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
    }
    Ok(v2)
}

pub fn initialize(root: &Path, state: &mut RoutedState, agent: &Agent) -> Result<bool> {
    state.agent_id.clone_from(&agent.id);
    state.project.clone_from(&agent.project);
    let directory = consumer_directory(root, &agent.id);
    let mut checkpoints = document_files_recursive(&directory);
    checkpoints.sort();
    if let Some(path) = checkpoints.last() {
        let checkpoint: Checkpoint = read_document(path)?;
        if checkpoint.schema == CHECKPOINT_SCHEMA && checkpoint.agent == agent.id {
            state.cursors = checkpoint.routes;
            return Ok(true);
        }
    }
    write_checkpoint(root, state)?;
    Ok(false)
}

pub fn publish(root: &Path, message: &Message) -> Result<Vec<PathBuf>> {
    let mut routes = message_routes(message);
    routes.sort();
    routes.dedup();
    let mut paths = Vec::with_capacity(routes.len());
    for route in routes {
        let path = message_path(root, &route, message)?;
        if path.exists() {
            let existing: Message = read_document(&path)?;
            if existing != *message {
                bail!("conflicting routed message {}", message.id);
            }
        } else {
            write_document(&path, message)?;
        }
        paths.push(path);
    }
    if let Some(expires_at) = message.expires_at.as_deref() {
        write_expiry(root, message, expires_at, &paths)?;
    }
    Ok(paths)
}

pub fn scan(
    root: &Path,
    state: &mut RoutedState,
    groups: impl Iterator<Item = String>,
) -> (Vec<Message>, Vec<String>) {
    let mut routes = vec![
        "global".to_owned(),
        format!("project:{}", state.project),
        format!("direct:{}", state.agent_id),
    ];
    routes.extend(groups.map(|group| format!("group:{group}")));
    let mut warnings = Vec::new();
    let mut messages = Vec::new();
    for route in routes {
        let cursor = state.cursors.get(&route).cloned();
        for path in recent_paths(root, &route) {
            if state.loaded.contains(&path) {
                continue;
            }
            match read_document::<Message>(&path) {
                Ok(message) => {
                    if is_expired(&message) {
                        state.loaded.insert(path);
                        continue;
                    }
                    if cursor.as_ref().is_none_or(|cursor| {
                        message.id > cursor.high_water || !cursor.overlap_ids.contains(&message.id)
                    }) {
                        messages.push(message);
                    }
                    state.warned.remove(&path);
                    state.loaded.insert(path);
                }
                Err(error) if state.warned.insert(path.clone()) => {
                    warnings.push(format!("read {}: {error:#}", path.display()));
                }
                Err(_) => {}
            }
        }
    }
    let loaded_floor = (Utc::now() - chrono::Duration::minutes(RECENT_MINUTES + 1))
        .timestamp_millis()
        .max(0) as u64;
    state
        .loaded
        .retain(|path| document_id(path).is_some_and(|id| id.timestamp_ms() >= loaded_floor));
    state.warned.retain(|path| path.exists());
    messages.sort_by(|left, right| left.id.cmp(&right.id));
    messages.dedup_by(|left, right| left.id == right.id);
    if state
        .last_gc
        .is_none_or(|last| last.elapsed() >= Duration::from_secs(600))
    {
        if let Err(error) = collect_expired(root) {
            warnings.push(format!("expiry collection: {error:#}"));
        }
        state.last_gc = Some(Instant::now());
    }
    (messages, warnings)
}

pub fn acknowledge(root: &Path, state: &mut RoutedState, messages: &[Message]) -> Result<()> {
    if messages.is_empty() || state.agent_id.is_empty() {
        return Ok(());
    }
    for message in messages {
        for route in message_routes_for_consumer(message, &state.agent_id, &state.project) {
            let cursor = state.cursors.entry(route).or_default();
            if cursor.high_water < message.id {
                cursor.high_water.clone_from(&message.id);
            }
            if !cursor.overlap_ids.contains(&message.id) {
                cursor.overlap_ids.push(message.id.clone());
            }
        }
    }
    let overlap_floor = (Utc::now() - chrono::Duration::minutes(RECENT_MINUTES))
        .timestamp_millis()
        .max(0) as u64;
    for cursor in state.cursors.values_mut() {
        cursor.overlap_ids.retain(|id| {
            id.parse::<Ulid>()
                .is_ok_and(|id| id.timestamp_ms() >= overlap_floor)
        });
        cursor.overlap_ids.sort();
        if cursor.overlap_ids.len() > CACHE_LIMIT {
            cursor
                .overlap_ids
                .drain(..cursor.overlap_ids.len() - CACHE_LIMIT);
        }
    }
    write_checkpoint(root, state)
}

fn write_checkpoint(root: &Path, state: &RoutedState) -> Result<()> {
    let id = Ulid::new().to_string();
    let checkpoint = Checkpoint {
        schema: CHECKPOINT_SCHEMA.to_owned(),
        id: id.clone(),
        agent: state.agent_id.clone(),
        updated_at: now(),
        routes: state.cursors.clone(),
    };
    let directory = consumer_directory(root, &state.agent_id);
    write_document(
        &directory.join(format!("{id}{DOCUMENT_SUFFIX}")),
        &checkpoint,
    )?;
    prune(&directory, 4);
    Ok(())
}

pub fn trim_cache(messages: &mut BTreeMap<String, Message>) {
    while messages.len() > CACHE_LIMIT {
        let Some(id) = messages.keys().next().cloned() else {
            break;
        };
        messages.remove(&id);
    }
}

pub fn history(
    root: &Path,
    agent: &Agent,
    groups: impl Iterator<Item = String>,
    thread: Option<&str>,
    group: Option<&str>,
    limit: usize,
) -> (Vec<Message>, Vec<String>) {
    let mut routes = vec![
        "global".to_owned(),
        format!("project:{}", agent.project),
        format!("direct:{}", agent.id),
    ];
    routes.extend(groups.map(|group| format!("group:{group}")));
    if let Some(group) = group {
        let expected = if group == "global" || group.starts_with("project:") {
            group.to_owned()
        } else {
            format!("group:{group}")
        };
        routes.retain(|route| route == &expected);
    }
    let mut paths = Vec::new();
    for route in routes {
        let route_root = route_directory(root, &route);
        let route_paths = if thread.is_some() {
            document_files_recursive(&route_root)
        } else {
            newest_paths(&route_root, limit)
        };
        paths.extend(route_paths);
    }
    let mut messages = Vec::new();
    let mut seen = HashSet::new();
    let mut warnings = Vec::new();
    for path in paths {
        match read_document::<Message>(&path) {
            Ok(message)
                if !is_expired(&message)
                    && thread.is_none_or(|thread| message.thread == thread)
                    && seen.insert(message.id.clone()) =>
            {
                messages.push(message);
            }
            Ok(_) => {}
            Err(error) => warnings.push(format!("read {}: {error:#}", path.display())),
        }
    }
    messages.sort_by(|left, right| left.id.cmp(&right.id));
    if messages.len() > limit {
        messages.drain(..messages.len() - limit);
    }
    (messages, warnings)
}

pub fn migrate_v1(root: &Path) -> Result<MigrationReport> {
    ensure(root)?;
    let legacy = root.join("v1/messages");
    let mut migrated = 0;
    let mut already_present = 0;
    for path in document_files_recursive(&legacy) {
        let message: Message = read_document(&path)?;
        let destinations = message_routes(&message)
            .into_iter()
            .map(|route| message_path(root, &route, &message))
            .collect::<Result<Vec<_>>>()?;
        let was_present = destinations.iter().all(|path| path.exists());
        publish(root, &message)?;
        if was_present {
            already_present += 1;
        } else {
            migrated += 1;
        }
    }
    let report = MigrationReport {
        schema: MIGRATION_SCHEMA.to_owned(),
        migrated,
        already_present,
        retained_legacy: true,
    };
    write_document(
        &root
            .join("v2/migrations")
            .join(format!("{}.json.zst", Ulid::new())),
        &report,
    )?;
    Ok(report)
}

fn message_routes(message: &Message) -> Vec<String> {
    if let Some(group) = message.group.as_deref() {
        if group == "global" {
            vec!["global".to_owned()]
        } else if let Some(project) = group.strip_prefix("project:") {
            vec![format!("project:{project}")]
        } else {
            vec![format!("group:{group}")]
        }
    } else {
        let mut agents = message.to.clone();
        agents.push(message.from.clone());
        agents
            .into_iter()
            .map(|agent| format!("direct:{agent}"))
            .collect()
    }
}

fn message_routes_for_consumer(message: &Message, agent: &str, project: &str) -> Vec<String> {
    message_routes(message)
        .into_iter()
        .filter(|route| {
            route == "global"
                || route == &format!("project:{project}")
                || route == &format!("direct:{agent}")
                || route.starts_with("group:")
        })
        .collect()
}

fn route_directory(root: &Path, route: &str) -> PathBuf {
    let v2 = root.join("v2/messages");
    if route == "global" {
        return v2.join("global");
    }
    let (kind, name) = route.split_once(':').expect("validated route");
    let collection = match kind {
        "project" => "projects",
        "group" => "groups",
        "direct" => "direct",
        _ => unreachable!("validated route kind"),
    };
    v2.join(collection).join(stable_shard(name)).join(name)
}

fn message_path(root: &Path, route: &str, message: &Message) -> Result<PathBuf> {
    let id: Ulid = message.id.parse().context("parse message ULID")?;
    let datetime = DateTime::<Utc>::from_timestamp_millis(id.timestamp_ms() as i64)
        .context("message ULID timestamp is out of range")?;
    Ok(route_directory(root, route)
        .join(format!("{:04}", datetime.year()))
        .join(format!("{:02}", datetime.month()))
        .join(format!("{:02}", datetime.day()))
        .join(format!("{:02}", datetime.hour()))
        .join(format!("{:02}", datetime.minute()))
        .join(message.id.chars().last().unwrap_or('0').to_string())
        .join(format!("{}{DOCUMENT_SUFFIX}", message.id)))
}

fn recent_paths(root: &Path, route: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for offset in 0..=RECENT_MINUTES {
        let time = Utc::now() - chrono::Duration::minutes(offset);
        let minute = route_directory(root, route)
            .join(format!("{:04}", time.year()))
            .join(format!("{:02}", time.month()))
            .join(format!("{:02}", time.day()))
            .join(format!("{:02}", time.hour()))
            .join(format!("{:02}", time.minute()));
        paths.extend(document_files_recursive(&minute));
    }
    paths
}

fn consumer_directory(root: &Path, agent: &str) -> PathBuf {
    root.join("v2/consumers")
        .join(stable_shard(agent))
        .join(agent)
}

fn write_expiry(root: &Path, message: &Message, expires_at: &str, paths: &[PathBuf]) -> Result<()> {
    let time = DateTime::parse_from_rfc3339(expires_at)?.with_timezone(&Utc);
    let expiry = Expiry {
        schema: EXPIRY_SCHEMA.to_owned(),
        message: message.id.clone(),
        expires_at: expires_at.to_owned(),
        paths: paths
            .iter()
            .map(|path| portable_relative_path(root, path))
            .collect(),
    };
    let path = root
        .join("v2/expiry")
        .join(format!("{:04}", time.year()))
        .join(format!("{:02}", time.month()))
        .join(format!("{:02}", time.day()))
        .join(format!("{:02}", time.hour()))
        .join(format!("{:02}", time.minute()))
        .join(format!("{}{DOCUMENT_SUFFIX}", message.id));
    write_document(&path, &expiry)
}

fn collect_expired(root: &Path) -> Result<()> {
    let now = Utc::now();
    let expiry_root = root.join("v2/expiry");
    for path in due_expiry_files(&expiry_root, now) {
        let expiry: Expiry = match read_document(&path) {
            Ok(expiry) => expiry,
            Err(_) => continue,
        };
        if expiry.schema != EXPIRY_SCHEMA
            || document_id(&path).map(|id| id.to_string()).as_deref()
                != Some(expiry.message.as_str())
        {
            bail!("expiry index path does not agree with its message identity");
        }
        let time = DateTime::parse_from_rfc3339(&expiry.expires_at)?.with_timezone(&Utc);
        if time <= now {
            for target in expiry.paths {
                match fs::remove_file(resolve_expiry_target(root, &target, &expiry.message)?) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            let parent = path.parent().map(Path::to_owned);
            let _ = fs::remove_file(&path);
            if let Some(parent) = parent {
                prune_empty_expiry_ancestors(&parent, &expiry_root);
            }
        }
    }
    Ok(())
}

fn resolve_expiry_target(root: &Path, stored: &str, message: &str) -> Result<PathBuf> {
    let normalized = stored.replace('\\', "/");
    let path = Path::new(&normalized);
    let components: Vec<_> = path.components().collect();
    if components
        .iter()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        bail!("expiry target contains a parent traversal");
    }
    let candidate = if path.starts_with(root) {
        path.to_owned()
    } else if let Some(index) = components
        .iter()
        .position(|component| component.as_os_str() == "v2")
    {
        components[index..]
            .iter()
            .fold(root.to_owned(), |path, component| {
                path.join(component.as_os_str())
            })
    } else if path.is_relative() {
        root.join(path)
    } else {
        bail!("expiry target is outside the board root");
    };
    let expected_name = format!("{message}{DOCUMENT_SUFFIX}");
    if !candidate.starts_with(root.join("v2/messages"))
        || candidate.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str())
    {
        bail!("expiry target is not the indexed message inside v2/messages");
    }
    Ok(candidate)
}

fn portable_relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn due_expiry_files(root: &Path, now: DateTime<Utc>) -> Vec<PathBuf> {
    let current = [
        format!("{:04}", now.year()),
        format!("{:02}", now.month()),
        format!("{:02}", now.day()),
        format!("{:02}", now.hour()),
        format!("{:02}", now.minute()),
    ];
    let mut pending = vec![(root.to_owned(), Vec::<String>::new())];
    let mut files = Vec::new();
    while let Some((directory, prefix)) = pending.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if prefix.len() == current.len() {
                if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(DOCUMENT_SUFFIX))
                {
                    files.push(path);
                }
                continue;
            }
            if !path.is_dir() {
                continue;
            }
            let Some(component) = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            let mut candidate = prefix.clone();
            candidate.push(component);
            if candidate.as_slice() <= &current[..candidate.len()] {
                pending.push((path, candidate));
            }
        }
    }
    files
}

fn prune_empty_expiry_ancestors(start: &Path, root: &Path) {
    let mut current = start.to_owned();
    while current != root {
        if fs::remove_dir(&current).is_err() {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_owned();
    }
}

fn is_expired(message: &Message) -> bool {
    message.expires_at.as_deref().is_some_and(|value| {
        DateTime::parse_from_rfc3339(value).is_ok_and(|time| time.with_timezone(&Utc) <= Utc::now())
    })
}

fn stable_shard(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:02x}", hash & 0xff)
}

fn document_files_recursive(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_owned()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(DOCUMENT_SUFFIX))
            {
                files.push(path);
            }
        }
    }
    files
}

fn newest_paths(route_root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut directories = vec![route_root.to_owned()];
    for _ in 0..5 {
        let mut next = Vec::new();
        for directory in directories {
            let Ok(entries) = fs::read_dir(directory) else {
                continue;
            };
            next.extend(
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir()),
            );
        }
        next.sort_by(|left, right| right.cmp(left));
        directories = next;
    }
    let mut paths = Vec::new();
    for minute in directories {
        paths.extend(document_files_recursive(&minute));
        if paths.len() >= limit {
            break;
        }
    }
    paths
}

fn document_id(path: &Path) -> Option<Ulid> {
    path.file_name()?
        .to_str()?
        .strip_suffix(DOCUMENT_SUFFIX)?
        .parse()
        .ok()
}

fn prune(directory: &Path, retain: usize) {
    let mut paths = document_files_recursive(directory);
    paths.sort();
    let remove = paths.len().saturating_sub(retain);
    for path in paths.into_iter().take(remove) {
        let _ = fs::remove_file(path);
    }
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn agent() -> Agent {
        Agent {
            schema: "aiboard.agent.v1".to_owned(),
            id: "infra-two".to_owned(),
            project: "infra".to_owned(),
            session_id: "two".to_owned(),
            platform: "linux".to_owned(),
            path: "/infra".to_owned(),
            registered_at: now(),
        }
    }

    fn message(id: String) -> Message {
        Message {
            schema: "aiboard.message.v1".to_owned(),
            id: id.clone(),
            timestamp: now(),
            from: "worka-one".to_owned(),
            to: vec!["infra-two".to_owned()],
            group: None,
            thread: id,
            reply_to: None,
            message: "hello".to_owned(),
            meta: None,
            expires_at: None,
        }
    }

    #[test]
    fn direct_message_is_routed_only_to_participants() {
        let root = tempdir().unwrap();
        ensure(root.path()).unwrap();
        let value = message(Ulid::new().to_string());
        let paths = publish(root.path(), &value).unwrap();
        assert_eq!(paths.len(), 2);
        assert!(
            paths
                .iter()
                .any(|path| path.to_string_lossy().contains("infra-two"))
        );
        assert!(
            !paths
                .iter()
                .any(|path| path.to_string_lossy().contains("other-three"))
        );
    }

    #[test]
    fn history_can_select_one_joined_group() {
        let root = tempdir().unwrap();
        ensure(root.path()).unwrap();
        let mut selected = message(Ulid::new().to_string());
        selected.to.clear();
        selected.group = Some("job-selected".to_owned());
        selected.message = "selected".to_owned();
        publish(root.path(), &selected).unwrap();
        let mut unrelated = message(Ulid::new().to_string());
        unrelated.to.clear();
        unrelated.group = Some("job-unrelated".to_owned());
        unrelated.message = "unrelated".to_owned();
        publish(root.path(), &unrelated).unwrap();

        let (messages, warnings) = history(
            root.path(),
            &agent(),
            ["job-selected".to_owned(), "job-unrelated".to_owned()].into_iter(),
            None,
            Some("job-selected"),
            50,
        );

        assert!(warnings.is_empty());
        assert_eq!(messages, vec![selected]);
    }

    #[test]
    fn checkpoint_resumes_and_overlap_delivers_late_message() {
        let root = tempdir().unwrap();
        ensure(root.path()).unwrap();
        let late = message(Ulid::new().to_string());
        let newer = message(Ulid::new().to_string());
        publish(root.path(), &newer).unwrap();

        let mut first = RoutedState::default();
        assert!(!initialize(root.path(), &mut first, &agent()).unwrap());
        let (received, warnings) = scan(root.path(), &mut first, std::iter::empty());
        assert!(warnings.is_empty());
        assert_eq!(received, vec![newer]);
        acknowledge(root.path(), &mut first, &received).unwrap();

        let mut resumed = RoutedState::default();
        assert!(initialize(root.path(), &mut resumed, &agent()).unwrap());
        assert!(
            scan(root.path(), &mut resumed, std::iter::empty())
                .0
                .is_empty()
        );

        publish(root.path(), &late).unwrap();
        assert_eq!(
            scan(root.path(), &mut resumed, std::iter::empty()).0,
            vec![late]
        );
    }

    #[test]
    fn expired_message_and_index_are_collected() {
        let root = tempdir().unwrap();
        ensure(root.path()).unwrap();
        let mut value = message(Ulid::new().to_string());
        value.expires_at = Some(
            (Utc::now() - chrono::Duration::seconds(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        );
        let paths = publish(root.path(), &value).unwrap();
        assert!(paths.iter().all(|path| path.exists()));

        collect_expired(root.path()).unwrap();
        assert!(paths.iter().all(|path| !path.exists()));
        assert!(document_files_recursive(&root.path().join("v2/expiry")).is_empty());
    }

    #[test]
    fn expiry_collection_does_not_visit_future_partitions() {
        let root = tempdir().unwrap();
        ensure(root.path()).unwrap();
        let mut value = message(Ulid::new().to_string());
        value.expires_at = Some(
            (Utc::now() + chrono::Duration::days(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        );
        let paths = publish(root.path(), &value).unwrap();

        collect_expired(root.path()).unwrap();

        assert!(paths.iter().all(|path| path.exists()));
        assert_eq!(
            document_files_recursive(&root.path().join("v2/expiry")).len(),
            1
        );
    }

    #[test]
    fn expiry_paths_survive_board_root_relocation() {
        let directory = tempdir().unwrap();
        let original = directory.path().join("debian-board");
        ensure(&original).unwrap();
        let mut value = message(Ulid::new().to_string());
        value.expires_at = Some(
            (Utc::now() - chrono::Duration::seconds(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        );
        publish(&original, &value).unwrap();
        let expiry_path = document_files_recursive(&original.join("v2/expiry"))
            .pop()
            .unwrap();
        let expiry: Expiry = read_document(&expiry_path).unwrap();
        assert!(
            expiry
                .paths
                .iter()
                .all(|path| { path.starts_with("v2/messages/") && !path.contains('\\') })
        );
        let relocated = directory.path().join("macos-board");
        fs::rename(&original, &relocated).unwrap();

        collect_expired(&relocated).unwrap();

        assert!(document_files_recursive(&relocated.join("v2/messages")).is_empty());
        assert!(document_files_recursive(&relocated.join("v2/expiry")).is_empty());
    }

    #[test]
    fn expiry_index_cannot_delete_outside_message_storage() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("board");
        ensure(&root).unwrap();
        let secret = directory.path().join("keep-me");
        fs::write(&secret, b"safe").unwrap();
        let id = Ulid::new().to_string();
        let expired = Utc::now() - chrono::Duration::seconds(1);
        let expiry = Expiry {
            schema: EXPIRY_SCHEMA.to_owned(),
            message: id.clone(),
            expires_at: expired.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            paths: vec![secret.to_string_lossy().into_owned()],
        };
        let index = root
            .join("v2/expiry")
            .join(format!("{:04}", expired.year()))
            .join(format!("{:02}", expired.month()))
            .join(format!("{:02}", expired.day()))
            .join(format!("{:02}", expired.hour()))
            .join(format!("{:02}", expired.minute()))
            .join(format!("{id}{DOCUMENT_SUFFIX}"));
        write_document(&index, &expiry).unwrap();

        assert!(collect_expired(&root).is_err());
        assert_eq!(fs::read(&secret).unwrap(), b"safe");
    }

    #[test]
    fn migration_is_idempotent_and_retains_legacy_messages() {
        let root = tempdir().unwrap();
        ensure(root.path()).unwrap();
        let value = message(Ulid::new().to_string());
        let legacy = root
            .path()
            .join("v1/messages/2026-09-06")
            .join(format!("{}.json.zst", value.id));
        write_document(&legacy, &value).unwrap();

        let first = migrate_v1(root.path()).unwrap();
        assert_eq!(first.migrated, 1);
        assert_eq!(first.already_present, 0);
        assert!(first.retained_legacy);
        assert!(legacy.exists());
        for route in message_routes(&value) {
            let destination = message_path(root.path(), &route, &value).unwrap();
            assert_eq!(read_document::<Message>(&destination).unwrap(), value);
        }

        let second = migrate_v1(root.path()).unwrap();
        assert_eq!(second.migrated, 0);
        assert_eq!(second.already_present, 1);
        assert!(legacy.exists());
    }
}

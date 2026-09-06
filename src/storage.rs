use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;
use ulid::{Generator, Ulid};

use crate::model::{
    AGENT_SCHEMA, Agent, AgentStatus, GROUP_SCHEMA, Group, MEMBERSHIP_SCHEMA, MESSAGE_SCHEMA,
    Membership, Message, PRESENCE_SCHEMA, Presence,
};

const DOCUMENT_SUFFIX: &str = ".json.zst";

thread_local! {
    static ULID_GENERATOR: RefCell<Generator> = const { RefCell::new(Generator::new()) };
}

#[derive(Default)]
pub struct BoardState {
    pub agents: BTreeMap<String, Agent>,
    pub groups: BTreeMap<String, Group>,
    pub memberships: HashSet<(String, String)>,
    pub messages: BTreeMap<String, Message>,
    pub presence: BTreeMap<String, Presence>,
    loaded_paths: HashSet<PathBuf>,
    loaded_presence_paths: HashSet<PathBuf>,
    warned_paths: HashSet<PathBuf>,
}

pub struct ScanResult {
    pub new_messages: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn ensure_layout(root: &Path) -> Result<PathBuf> {
    let version_root = root.join("v1");
    for directory in [
        version_root.join("agents"),
        version_root.join("groups"),
        version_root.join("messages"),
        version_root.join("presence"),
    ] {
        fs::create_dir_all(&directory)
            .with_context(|| format!("create board directory {}", directory.display()))?;
    }
    Ok(version_root)
}

pub fn normalize_session(session: &str) -> Result<String> {
    let normalized: String = session
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect();
    if normalized.is_empty() {
        bail!("session ID must contain an ASCII letter or number");
    }
    Ok(normalized)
}

pub fn validate_slug(value: &str, kind: &str) -> Result<()> {
    if value.is_empty()
        || !value.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || character == '-'
                || character == '_'
        })
    {
        bail!("{kind} must contain only lowercase ASCII letters, numbers, '-' or '_'");
    }
    Ok(())
}

pub fn register_agent(
    version_root: &Path,
    project: &str,
    session_id: &str,
    platform: &str,
    project_path: &Path,
) -> Result<Agent> {
    validate_slug(project, "project")?;
    let session_key = normalize_session(session_id)?;
    let id = format!("{project}-{session_key}");
    let path = version_root
        .join("agents")
        .join(project)
        .join(format!("{session_key}{DOCUMENT_SUFFIX}"));

    if path.exists() {
        let existing: Agent = read_document(&path)?;
        validate_agent(&existing, project, &session_key)?;
        if existing.session_id != session_id {
            bail!("agent registration conflicts with an existing normalized session ID");
        }
        return Ok(existing);
    }

    let agent = Agent {
        schema: AGENT_SCHEMA.to_owned(),
        id,
        project: project.to_owned(),
        session_id: session_id.to_owned(),
        platform: platform.to_owned(),
        path: project_path.to_string_lossy().into_owned(),
        registered_at: timestamp(),
    };
    write_document(&path, &agent)?;
    Ok(agent)
}

pub fn create_group(version_root: &Path, name: &str, agent_id: &str) -> Result<Group> {
    validate_slug(name, "group")?;
    if name == "global" {
        bail!("global is a built-in group");
    }
    let path = version_root
        .join("groups")
        .join(name)
        .join(format!("group{DOCUMENT_SUFFIX}"));
    if path.exists() {
        let group: Group = read_document(&path)?;
        validate_group(&group, name)?;
        return Ok(group);
    }
    let group = Group {
        schema: GROUP_SCHEMA.to_owned(),
        name: name.to_owned(),
        created_by: agent_id.to_owned(),
        created_at: timestamp(),
    };
    write_document(&path, &group)?;
    Ok(group)
}

pub fn join_group(version_root: &Path, name: &str, agent_id: &str) -> Result<Membership> {
    validate_slug(name, "group")?;
    let group_path = version_root
        .join("groups")
        .join(name)
        .join(format!("group{DOCUMENT_SUFFIX}"));
    if !group_path.exists() {
        bail!("unknown group '{name}'");
    }
    let membership_path = version_root
        .join("groups")
        .join(name)
        .join("members")
        .join(format!("{agent_id}{DOCUMENT_SUFFIX}"));
    if membership_path.exists() {
        let membership: Membership = read_document(&membership_path)?;
        validate_membership(&membership, name, agent_id)?;
        return Ok(membership);
    }
    let membership = Membership {
        schema: MEMBERSHIP_SCHEMA.to_owned(),
        group: name.to_owned(),
        agent: agent_id.to_owned(),
        joined_at: timestamp(),
    };
    write_document(&membership_path, &membership)?;
    Ok(membership)
}

pub fn publish_message(
    version_root: &Path,
    from: &str,
    to: Vec<String>,
    group: Option<String>,
    thread: Option<String>,
    reply_to: Option<String>,
    body: String,
) -> Result<Message> {
    let has_recipients = !to.is_empty();
    if has_recipients == group.is_some() {
        bail!("provide direct recipients or one group, but not both");
    }
    if body.trim().is_empty() {
        bail!("message must not be empty");
    }

    let id = next_ulid().to_string();
    let message = Message {
        schema: MESSAGE_SCHEMA.to_owned(),
        id: id.clone(),
        timestamp: timestamp(),
        from: from.to_owned(),
        to,
        group,
        thread: thread.unwrap_or_else(|| id.clone()),
        reply_to,
        message: body,
    };
    let path = version_root
        .join("messages")
        .join(Utc::now().format("%Y-%m-%d").to_string())
        .join(format!("{}{DOCUMENT_SUFFIX}", message.id));
    write_document(&path, &message)?;
    Ok(message)
}

pub fn touch_presence(version_root: &Path, agent_id: &str) -> Result<Presence> {
    let id = next_ulid().to_string();
    let presence = Presence {
        schema: PRESENCE_SCHEMA.to_owned(),
        id: id.clone(),
        agent: agent_id.to_owned(),
        last_seen: timestamp(),
    };
    let directory = version_root.join("presence").join(agent_id);
    let path = directory.join(format!("{id}{DOCUMENT_SUFFIX}"));
    write_document(&path, &presence)?;
    prune_presence(&directory, 4);
    Ok(presence)
}

pub fn scan(version_root: &Path, state: &mut BoardState) -> ScanResult {
    let mut result = ScanResult {
        new_messages: Vec::new(),
        warnings: Vec::new(),
    };
    scan_agents(version_root, state, &mut result);
    scan_groups(version_root, state, &mut result);
    scan_presence(version_root, state, &mut result);
    scan_messages(version_root, state, &mut result);
    result
}

fn scan_presence(version_root: &Path, state: &mut BoardState, result: &mut ScanResult) {
    state.loaded_presence_paths.retain(|path| path.exists());
    let presence_root = version_root.join("presence");
    for agent_dir in child_directories(&presence_root, result) {
        let Some(agent_id) = agent_dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        for path in document_files(&agent_dir, result) {
            if state.loaded_presence_paths.contains(&path) {
                continue;
            }
            let Some(id) = document_name(&path) else {
                continue;
            };
            match read_document::<Presence>(&path)
                .and_then(|presence| validate_presence(&presence, id, agent_id).map(|()| presence))
            {
                Ok(presence) => {
                    let replace = state
                        .presence
                        .get(agent_id)
                        .is_none_or(|current| current.id < presence.id);
                    if replace {
                        state.presence.insert(agent_id.to_owned(), presence);
                    }
                    state.warned_paths.remove(&path);
                    state.loaded_presence_paths.insert(path);
                }
                Err(error) => warn_path(state, result, path, error),
            }
        }
    }
}

fn scan_agents(version_root: &Path, state: &mut BoardState, result: &mut ScanResult) {
    let agents_root = version_root.join("agents");
    for project_dir in child_directories(&agents_root, result) {
        let Some(project) = project_dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        for path in document_files(&project_dir, result) {
            if state.loaded_paths.contains(&path) {
                continue;
            }
            let Some(session_key) = document_name(&path) else {
                continue;
            };
            match read_document::<Agent>(&path)
                .and_then(|agent| validate_agent(&agent, project, session_key).map(|()| agent))
            {
                Ok(agent) => {
                    state.agents.insert(agent.id.clone(), agent);
                    accept_path(state, path);
                }
                Err(error) => warn_path(state, result, path, error),
            }
        }
    }
}

fn scan_groups(version_root: &Path, state: &mut BoardState, result: &mut ScanResult) {
    let groups_root = version_root.join("groups");
    for group_dir in child_directories(&groups_root, result) {
        let Some(name) = group_dir.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let group_path = group_dir.join(format!("group{DOCUMENT_SUFFIX}"));
        if group_path.exists() && !state.loaded_paths.contains(&group_path) {
            match read_document::<Group>(&group_path)
                .and_then(|group| validate_group(&group, name).map(|()| group))
            {
                Ok(group) => {
                    state.groups.insert(group.name.clone(), group);
                    accept_path(state, group_path);
                }
                Err(error) => warn_path(state, result, group_path, error),
            }
        }

        let members_root = group_dir.join("members");
        for path in document_files(&members_root, result) {
            if state.loaded_paths.contains(&path) {
                continue;
            }
            let Some(agent_id) = document_name(&path) else {
                continue;
            };
            match read_document::<Membership>(&path).and_then(|membership| {
                validate_membership(&membership, name, agent_id).map(|()| membership)
            }) {
                Ok(membership) => {
                    state
                        .memberships
                        .insert((membership.group, membership.agent));
                    accept_path(state, path);
                }
                Err(error) => warn_path(state, result, path, error),
            }
        }
    }
}

fn scan_messages(version_root: &Path, state: &mut BoardState, result: &mut ScanResult) {
    let messages_root = version_root.join("messages");
    for date_dir in child_directories(&messages_root, result) {
        for path in document_files(&date_dir, result) {
            if state.loaded_paths.contains(&path) {
                continue;
            }
            let Some(id) = document_name(&path) else {
                continue;
            };
            match read_document::<Message>(&path)
                .and_then(|message| validate_message(&message, id).map(|()| message))
            {
                Ok(message) => {
                    let message_id = message.id.clone();
                    state.messages.insert(message_id.clone(), message);
                    result.new_messages.push(message_id);
                    accept_path(state, path);
                }
                Err(error) => warn_path(state, result, path, error),
            }
        }
    }
    result.new_messages.sort();
}

fn child_directories(root: &Path, result: &mut ScanResult) -> Vec<PathBuf> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            result
                .warnings
                .push(format!("scan {}: {error}", root.display()));
            return Vec::new();
        }
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect()
}

fn document_files(root: &Path, result: &mut ScanResult) -> Vec<PathBuf> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            result
                .warnings
                .push(format!("scan {}: {error}", root.display()));
            return Vec::new();
        }
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(DOCUMENT_SUFFIX))
        })
        .collect()
}

fn accept_path(state: &mut BoardState, path: PathBuf) {
    state.warned_paths.remove(&path);
    state.loaded_paths.insert(path);
}

fn warn_path(state: &mut BoardState, result: &mut ScanResult, path: PathBuf, error: anyhow::Error) {
    if state.warned_paths.insert(path.clone()) {
        result
            .warnings
            .push(format!("read {}: {error:#}", path.display()));
    }
}

fn validate_agent(agent: &Agent, project: &str, session_key: &str) -> Result<()> {
    if agent.schema != AGENT_SCHEMA {
        bail!("unsupported agent schema '{}'", agent.schema);
    }
    if agent.project != project || normalize_session(&agent.session_id)? != session_key {
        bail!("agent path does not agree with document identity");
    }
    if agent.id != format!("{project}-{session_key}") {
        bail!("agent ID does not agree with project and session");
    }
    Ok(())
}

fn validate_group(group: &Group, name: &str) -> Result<()> {
    if group.schema != GROUP_SCHEMA || group.name != name {
        bail!("group path does not agree with document identity");
    }
    Ok(())
}

fn validate_membership(membership: &Membership, group: &str, agent: &str) -> Result<()> {
    if membership.schema != MEMBERSHIP_SCHEMA
        || membership.group != group
        || membership.agent != agent
    {
        bail!("membership path does not agree with document identity");
    }
    Ok(())
}

fn validate_message(message: &Message, id: &str) -> Result<()> {
    if message.schema != MESSAGE_SCHEMA || message.id != id {
        bail!("message path does not agree with document identity");
    }
    message
        .id
        .parse::<Ulid>()
        .map_err(|error| anyhow!("invalid message ULID: {error}"))?;
    if message.to.is_empty() == message.group.is_none() {
        bail!("message must target direct recipients or one group");
    }
    Ok(())
}

fn validate_presence(presence: &Presence, id: &str, agent_id: &str) -> Result<()> {
    if presence.schema != PRESENCE_SCHEMA || presence.id != id || presence.agent != agent_id {
        bail!("presence path does not agree with document identity");
    }
    presence
        .id
        .parse::<Ulid>()
        .map_err(|error| anyhow!("invalid presence ULID: {error}"))?;
    DateTime::parse_from_rfc3339(&presence.last_seen)
        .map_err(|error| anyhow!("invalid presence timestamp: {error}"))?;
    Ok(())
}

fn prune_presence(directory: &Path, retain: usize) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut paths: Vec<_> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(DOCUMENT_SUFFIX))
        })
        .collect();
    paths.sort();
    let remove_count = paths.len().saturating_sub(retain);
    for path in paths.into_iter().take(remove_count) {
        let _ = fs::remove_file(path);
    }
}

fn document_name(path: &Path) -> Option<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(DOCUMENT_SUFFIX))
}

pub fn read_document<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(BufReader::new(file))
        .with_context(|| format!("open Zstandard frame {}", path.display()))?;
    serde_json::from_reader(decoder).with_context(|| format!("decode JSON {}", path.display()))
}

pub fn write_document<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("document path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create document directory {}", parent.display()))?;

    let final_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("document path is not UTF-8: {}", path.display()))?;
    let temporary_path = parent.join(format!(".{}.{:016x}.part", final_name, random_suffix()));
    let temporary = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary_path)
        .with_context(|| format!("create temporary document {}", temporary_path.display()))?;

    let result = (|| -> Result<()> {
        let writer = BufWriter::new(temporary);
        let mut encoder =
            zstd::stream::write::Encoder::new(writer, 3).context("create Zstandard encoder")?;
        encoder.include_checksum(true)?;
        serde_json::to_writer(&mut encoder, value).context("encode document JSON")?;
        let mut writer = encoder.finish().context("finish Zstandard frame")?;
        writer.flush().context("flush compressed document")?;
        let file = writer
            .into_inner()
            .map_err(|error| anyhow!("close compressed document: {error}"))?;
        synchronize_document(&file)?;
        fs::rename(&temporary_path, path).with_context(|| {
            format!(
                "publish document {} as {}",
                temporary_path.display(),
                path.display()
            )
        })?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn synchronize_document(file: &File) -> Result<()> {
    match file.sync_all() {
        Ok(()) => Ok(()),
        // macOS can expose the shared WebDAV project volume through a file
        // descriptor that rejects fsync with ENOTTY. The completed Zstandard
        // checksum plus atomic sibling rename remains the protocol's reader
        // safety boundary on that filesystem.
        Err(error) if error.raw_os_error() == Some(25) => Ok(()),
        Err(error) => Err(error).context("synchronize compressed document"),
    }
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn random_suffix() -> u64 {
    let bytes = Ulid::new().to_bytes();
    u64::from_be_bytes(bytes[8..].try_into().expect("ULID suffix is eight bytes"))
}

fn next_ulid() -> Ulid {
    ULID_GENERATOR.with(|generator| {
        generator
            .borrow_mut()
            .generate()
            .unwrap_or_else(|_| Ulid::new())
    })
}

pub fn group_members<'a>(state: &'a BoardState, group: &str) -> Vec<&'a str> {
    let mut members: Vec<_> = state
        .memberships
        .iter()
        .filter_map(|(candidate_group, agent)| (candidate_group == group).then_some(agent.as_str()))
        .collect();
    members.sort_unstable();
    members
}

pub fn agent_statuses(state: &BoardState) -> Vec<AgentStatus> {
    let now = Utc::now();
    state
        .agents
        .values()
        .map(|agent| {
            let presence = state.presence.get(&agent.id);
            let age_seconds = presence.and_then(|presence| {
                DateTime::parse_from_rfc3339(&presence.last_seen)
                    .ok()
                    .map(|seen| {
                        now.signed_duration_since(seen.with_timezone(&Utc))
                            .num_seconds()
                    })
                    .map(|age| age.max(0) as u64)
            });
            AgentStatus {
                agent: agent.clone(),
                online: age_seconds.is_some_and(|age| age <= 60),
                last_seen: presence.map(|presence| presence.last_seen.clone()),
                age_seconds,
            }
        })
        .collect()
}

pub fn message_is_relevant(state: &BoardState, agent: &Agent, message: &Message) -> bool {
    if message.from == agent.id || message.to.iter().any(|recipient| recipient == &agent.id) {
        return message.from != agent.id;
    }
    match message.group.as_deref() {
        Some("global") => true,
        Some(group) if group == format!("project:{}", agent.project) => true,
        Some(group) => state
            .memberships
            .contains(&(group.to_owned(), agent.id.clone())),
        None => false,
    }
}

pub fn direct_reply_recipients(message: &Message, replying_agent: &str) -> Vec<String> {
    let mut recipients = message.to.clone();
    recipients.push(message.from.clone());
    recipients.sort();
    recipients.dedup();
    recipients.retain(|participant| participant != replying_agent);
    recipients
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn compressed_document_round_trip() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("message.json.zst");
        let value = vec!["hello", "shared", "board"];
        write_document(&path, &value).unwrap();
        assert_eq!(read_document::<Vec<String>>(&path).unwrap(), value);
        assert!(
            !directory.path().read_dir().unwrap().any(|entry| entry
                .unwrap()
                .path()
                .to_string_lossy()
                .ends_with(".part"))
        );
    }

    #[test]
    fn registration_is_idempotent() {
        let directory = tempdir().unwrap();
        let root = ensure_layout(directory.path()).unwrap();
        let first =
            register_agent(&root, "infra", "session-123", "linux", Path::new("/one")).unwrap();
        let second =
            register_agent(&root, "infra", "session-123", "linux", Path::new("/two")).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.id, "infra-session123");
    }

    #[test]
    fn presence_expires_after_one_minute() {
        let directory = tempdir().unwrap();
        let root = ensure_layout(directory.path()).unwrap();
        let agent =
            register_agent(&root, "infra", "session", "linux", Path::new("/infra")).unwrap();
        let current = touch_presence(&root, &agent.id).unwrap();
        let mut state = BoardState::default();
        scan(&root, &mut state);
        let status = agent_statuses(&state).pop().unwrap();
        assert!(status.online);
        assert_eq!(
            status.last_seen.as_deref(),
            Some(current.last_seen.as_str())
        );

        state.presence.insert(
            agent.id.clone(),
            Presence {
                schema: PRESENCE_SCHEMA.to_owned(),
                id: Ulid::new().to_string(),
                agent: agent.id,
                last_seen: (Utc::now() - chrono::Duration::seconds(61))
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            },
        );
        assert!(!agent_statuses(&state).pop().unwrap().online);
    }

    #[test]
    fn corrupt_document_is_retried_after_replacement() {
        let directory = tempdir().unwrap();
        let root = ensure_layout(directory.path()).unwrap();
        let message_dir = root.join("messages").join("2026-09-06");
        fs::create_dir_all(&message_dir).unwrap();
        let id = Ulid::new().to_string();
        let path = message_dir.join(format!("{id}{DOCUMENT_SUFFIX}"));
        fs::write(&path, b"not zstd").unwrap();

        let mut state = BoardState::default();
        assert_eq!(scan(&root, &mut state).warnings.len(), 1);
        assert_eq!(scan(&root, &mut state).warnings.len(), 0);

        let message = Message {
            schema: MESSAGE_SCHEMA.to_owned(),
            id: id.clone(),
            timestamp: timestamp(),
            from: "worka-a".to_owned(),
            to: vec!["infra-b".to_owned()],
            group: None,
            thread: id.clone(),
            reply_to: None,
            message: "fixed".to_owned(),
        };
        write_document(&path, &message).unwrap();
        assert_eq!(scan(&root, &mut state).new_messages, vec![id]);
    }

    #[test]
    fn relevance_covers_direct_virtual_and_custom_groups() {
        let agent = Agent {
            schema: AGENT_SCHEMA.to_owned(),
            id: "infra-a".to_owned(),
            project: "infra".to_owned(),
            session_id: "a".to_owned(),
            platform: "linux".to_owned(),
            path: "/infra".to_owned(),
            registered_at: timestamp(),
        };
        let mut state = BoardState::default();
        state
            .memberships
            .insert(("storage".to_owned(), agent.id.clone()));
        let make = |to: Vec<&str>, group: Option<&str>| Message {
            schema: MESSAGE_SCHEMA.to_owned(),
            id: Ulid::new().to_string(),
            timestamp: timestamp(),
            from: "worka-b".to_owned(),
            to: to.into_iter().map(str::to_owned).collect(),
            group: group.map(str::to_owned),
            thread: Ulid::new().to_string(),
            reply_to: None,
            message: "hello".to_owned(),
        };
        assert!(message_is_relevant(
            &state,
            &agent,
            &make(vec!["infra-a"], None)
        ));
        assert!(message_is_relevant(
            &state,
            &agent,
            &make(vec![], Some("global"))
        ));
        assert!(message_is_relevant(
            &state,
            &agent,
            &make(vec![], Some("project:infra"))
        ));
        assert!(message_is_relevant(
            &state,
            &agent,
            &make(vec![], Some("storage"))
        ));
        assert!(!message_is_relevant(
            &state,
            &agent,
            &make(vec![], Some("other"))
        ));
    }
}

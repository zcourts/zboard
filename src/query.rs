//! Read-only discovery and message queries. These never advance listener cursors.
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::model::{Agent, AgentStatus, MESSAGE_SCHEMA, Message};
use crate::routed::{message_id_path, route_directory};
use crate::storage::{BoardState, agent_statuses, group_members, read_document, scan_metadata};

fn default_limit() -> usize {
    50
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct HistoryQuery {
    pub thread: Option<String>,
    pub group: Option<String>,
    pub sender: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Case-insensitive substring of the message body, not arbitrary metadata.
    pub search: Option<String>,
    /// Exclusive upper ULID bound. Results are newest-first.
    pub before: Option<String>,
    /// Exclusive lower ULID bound. Results are oldest-first when supplied.
    pub after: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

impl Default for HistoryQuery {
    fn default() -> Self {
        Self {
            thread: None,
            group: None,
            sender: None,
            tags: Vec::new(),
            search: None,
            before: None,
            after: None,
            limit: default_limit(),
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MessagePage {
    pub messages: Vec<Message>,
    /// Pass as `before` for older pages or `after` for forward pages.
    /// Present only when an additional matching message was observed.
    pub next_cursor: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MessageLookup {
    pub messages: Vec<Message>,
    /// Missing, expired, or outside this identity's relevant routes.
    pub missing: Vec<String>,
}

#[derive(Default, Debug, Deserialize, JsonSchema)]
pub struct AgentQuery {
    pub project: Option<String>,
    pub online: Option<bool>,
    pub search: Option<String>,
    pub limit: Option<usize>,
    pub after: Option<String>,
}

#[derive(Default, Debug, Deserialize, JsonSchema)]
pub struct GroupQuery {
    #[serde(default)]
    pub joined: bool,
    pub search: Option<String>,
    pub limit: Option<usize>,
    pub after: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentPage {
    pub agents: Vec<AgentStatus>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GroupPage {
    pub groups: Vec<OwnedGroupSummary>,
    pub next_cursor: Option<String>,
}

fn discovery_limit(limit: Option<usize>) -> Result<usize> {
    let limit = limit.unwrap_or(100);
    if limit == 0 || limit > 1000 {
        bail!("limit must be between 1 and 1000");
    }
    Ok(limit)
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct OwnedGroupSummary {
    pub name: String,
    pub created_by: String,
    pub members: Vec<String>,
    pub joined: bool,
}

pub fn snapshot(root: &Path) -> Result<BoardState> {
    let mut state = BoardState::default();
    let result = scan_metadata(&root.join("v1"), &mut state);
    if !result.warnings.is_empty() {
        bail!("board metadata unavailable: {}", result.warnings.join("; "));
    }
    Ok(state)
}

pub fn agents(state: &BoardState, query: &AgentQuery) -> Result<AgentPage> {
    let limit = discovery_limit(query.limit)?;
    let mut agents: Vec<_> = agent_statuses(state)
        .into_iter()
        .filter(|item| {
            query
                .project
                .as_ref()
                .is_none_or(|p| item.agent.project == *p)
                && query.online.is_none_or(|online| item.online == online)
                && query
                    .after
                    .as_ref()
                    .is_none_or(|after| item.agent.id > *after)
                && query
                    .search
                    .as_ref()
                    .is_none_or(|s| item.agent.id.contains(s) || item.agent.path.contains(s))
        })
        .take(limit + 1)
        .collect();
    let more = agents.len() > limit;
    agents.truncate(limit);
    let next_cursor = more.then(|| agents.last().unwrap().agent.id.clone());
    Ok(AgentPage {
        agents,
        next_cursor,
    })
}

pub fn groups(state: &BoardState, agent: &Agent, query: &GroupQuery) -> Result<GroupPage> {
    let limit = discovery_limit(query.limit)?;
    let mut summaries = vec![
        OwnedGroupSummary {
            name: "global".into(),
            created_by: "builtin".into(),
            members: Vec::new(),
            joined: true,
        },
        OwnedGroupSummary {
            name: format!("project:{}", agent.project),
            created_by: "builtin".into(),
            members: state
                .agents
                .values()
                .filter(|a| a.project == agent.project)
                .map(|a| a.id.clone())
                .collect(),
            joined: true,
        },
    ];
    summaries.extend(state.groups.values().map(|group| {
        OwnedGroupSummary {
            name: group.name.clone(),
            created_by: group.created_by.clone(),
            members: group_members(state, &group.name)
                .into_iter()
                .map(str::to_owned)
                .collect(),
            joined: state
                .memberships
                .contains(&(group.name.clone(), agent.id.clone())),
        }
    }));
    summaries.retain(|g| {
        (!query.joined || g.joined)
            && query.search.as_ref().is_none_or(|s| g.name.contains(s))
            && query.after.as_ref().is_none_or(|after| g.name > *after)
    });
    summaries.sort_by(|a, b| a.name.cmp(&b.name));
    let more = summaries.len() > limit;
    summaries.truncate(limit);
    let next_cursor = more.then(|| summaries.last().unwrap().name.clone());
    Ok(GroupPage {
        groups: summaries,
        next_cursor,
    })
}

fn routes(state: &BoardState, agent: &Agent, group: Option<&str>) -> Vec<String> {
    let mut routes = vec![
        "global".into(),
        format!("project:{}", agent.project),
        format!("direct:{}", agent.id),
    ];
    routes.extend(
        state
            .memberships
            .iter()
            .filter_map(|(g, a)| (a == &agent.id).then(|| format!("group:{g}"))),
    );
    if let Some(group) = group {
        let route = if group == "global" || group.starts_with("project:") {
            group.to_owned()
        } else {
            format!("group:{group}")
        };
        routes.retain(|r| r == &route);
    }
    routes.sort();
    routes.dedup();
    routes
}

fn readable(message: &Message, state: &BoardState, agent: &Agent) -> bool {
    if message.schema != MESSAGE_SCHEMA {
        return false;
    }
    if let Some(expiry) = &message.expires_at {
        let Ok(expiry) = DateTime::parse_from_rfc3339(expiry) else {
            return false;
        };
        if expiry <= Utc::now() {
            return false;
        }
    }
    message.from == agent.id || crate::storage::message_is_relevant(state, agent, message)
}

fn canonical_id(id: &str) -> Result<String> {
    Ok(id
        .parse::<Ulid>()
        .context("invalid message ULID")?
        .to_string())
}

pub fn messages_get(
    root: &Path,
    state: &BoardState,
    agent: &Agent,
    ids: &[String],
) -> Result<MessageLookup> {
    if ids.is_empty() || ids.len() > 100 {
        bail!("supply between 1 and 100 message IDs");
    }
    let routes = routes(state, agent, None);
    let mut result = MessageLookup {
        messages: Vec::new(),
        missing: Vec::new(),
    };
    let mut seen = HashSet::new();
    for id in ids {
        let id = canonical_id(id)?;
        if !seen.insert(id.clone()) {
            continue;
        }
        let mut found = None;
        for route in &routes {
            let path = message_id_path(root, route, &id)?;
            if !path.exists() {
                continue;
            }
            let message: Message =
                read_document(&path).with_context(|| format!("read message {id}"))?;
            if message.id != id {
                bail!("message file identity mismatch: {id}");
            }
            if readable(&message, state, agent) {
                found = Some(message);
                break;
            }
        }
        if let Some(message) = found {
            result.messages.push(message);
        } else {
            result.missing.push(id);
        }
    }
    Ok(result)
}

/// Walk one time-partitioned route in ID order, loading at most one minute's
/// filenames at once. Directory traversal never follows symlinks.
struct RouteFiles {
    pending: Vec<(PathBuf, usize)>,
    files: Vec<PathBuf>,
    ascending: bool,
    lower: Option<PathBuf>,
    upper: Option<PathBuf>,
}

impl RouteFiles {
    fn new(
        path: PathBuf,
        ascending: bool,
        after: Option<&str>,
        before: Option<&str>,
    ) -> Result<Self> {
        let minute = |id: &str| -> Result<PathBuf> {
            let id: Ulid = id.parse()?;
            let time = DateTime::<Utc>::from_timestamp_millis(id.timestamp_ms() as i64)
                .context("cursor timestamp out of range")?;
            Ok(path.join(time.format("%Y/%m/%d/%H/%M").to_string()))
        };
        let lower = after.map(minute).transpose()?;
        let upper = before.map(minute).transpose()?;
        Ok(Self {
            pending: vec![(path, 0)],
            files: Vec::new(),
            ascending,
            lower,
            upper,
        })
    }
    fn next(&mut self) -> Result<Option<PathBuf>> {
        loop {
            if let Some(path) = self.files.pop() {
                return Ok(Some(path));
            }
            let Some((directory, depth)) = self.pending.pop() else {
                return Ok(None);
            };
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e).with_context(|| format!("list {}", directory.display())),
            };
            let mut paths = Vec::new();
            for entry in entries {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    paths.push(entry.path());
                }
            }
            if depth == 5 {
                for shard in paths {
                    for entry in fs::read_dir(shard)? {
                        let entry = entry?;
                        if entry.file_type()?.is_file()
                            && entry.file_name().to_string_lossy().ends_with(".json.zst")
                        {
                            self.files.push(entry.path());
                        }
                    }
                }
                self.files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
                if self.ascending {
                    self.files.reverse();
                }
            } else {
                // Skip whole time partitions outside the cursor window rather
                // than enumerating every historical message on each poll.
                paths.retain(|path| {
                    let remaining = 4 - depth;
                    !self.lower.as_ref().is_some_and(|bound| {
                        path.as_path() < bound.ancestors().nth(remaining).unwrap()
                    }) && !self.upper.as_ref().is_some_and(|bound| {
                        path.as_path() > bound.ancestors().nth(remaining).unwrap()
                    })
                });
                paths.sort();
                if self.ascending {
                    paths.reverse();
                }
                self.pending
                    .extend(paths.into_iter().map(|p| (p, depth + 1)));
            }
        }
    }
}

pub fn history(
    root: &Path,
    state: &BoardState,
    agent: &Agent,
    query: &HistoryQuery,
) -> Result<MessagePage> {
    if query.limit == 0 || query.limit > 1000 {
        bail!("limit must be between 1 and 1000");
    }
    let before = query.before.as_deref().map(canonical_id).transpose()?;
    let after = query.after.as_deref().map(canonical_id).transpose()?;
    if let (Some(before), Some(after)) = (&before, &after) {
        if after >= before {
            bail!("after must precede before");
        }
    }
    let ascending = after.is_some();
    let search = query.search.as_ref().map(|s| s.to_lowercase());
    let mut found = BTreeMap::new();
    for route in routes(state, agent, query.group.as_deref()) {
        let mut files = RouteFiles::new(
            route_directory(root, &route),
            ascending,
            after.as_deref(),
            before.as_deref(),
        )?;
        let mut matched = 0;
        while let Some(path) = files.next()? {
            let Some(id) = path
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_suffix(".json.zst"))
            else {
                continue;
            };
            if before.as_ref().is_some_and(|b| id >= b.as_str())
                || after.as_ref().is_some_and(|a| id <= a.as_str())
            {
                continue;
            }
            let message: Message =
                read_document(&path).with_context(|| format!("read {}", path.display()))?;
            if message.id != id {
                bail!("message file identity mismatch: {id}");
            }
            if !readable(&message, state, agent)
                || query.thread.as_ref().is_some_and(|s| message.thread != *s)
                || query.sender.as_ref().is_some_and(|s| message.from != *s)
                || !query.tags.iter().all(|s| message.tags.contains(s))
                || search
                    .as_ref()
                    .is_some_and(|s| !message.message.to_lowercase().contains(s))
            {
                continue;
            }
            found.insert(message.id.clone(), message);
            matched += 1;
            if matched > query.limit {
                break;
            }
        }
        while found.len() > query.limit + 1 {
            if ascending {
                found.pop_last();
            } else {
                found.pop_first();
            }
        }
    }
    let mut messages: Vec<_> = found.into_values().collect();
    if !ascending {
        messages.reverse();
    }
    let more = messages.len() > query.limit;
    messages.truncate(query.limit);
    let next_cursor = more.then(|| messages.last().unwrap().id.clone());
    Ok(MessagePage {
        messages,
        next_cursor,
        warnings: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{MessageDraft, ensure_layout, publish_message, register_agent};

    fn draft(body: &str, to: Vec<String>, group: Option<String>) -> MessageDraft {
        MessageDraft {
            to,
            group,
            thread: None,
            reply_to: None,
            body: body.into(),
            tags: vec!["release".into()],
            meta: None,
            ttl_seconds: None,
        }
    }

    #[test]
    fn exact_lookup_filters_private_routes_and_never_creates_consumers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let v1 = ensure_layout(root).unwrap();
        let a = register_agent(&v1, "sample", "one", "linux", root).unwrap();
        let b = register_agent(&v1, "sample", "two", "linux", root).unwrap();
        let private =
            publish_message(&v1, &b.id, draft("private", vec!["elsewhere".into()], None)).unwrap();
        let global = publish_message(
            &v1,
            &b.id,
            draft("public", Vec::new(), Some("global".into())),
        )
        .unwrap();
        let state = snapshot(root).unwrap();
        let result =
            messages_get(root, &state, &a, &[private.id.clone(), global.id.clone()]).unwrap();
        assert_eq!(result.messages, vec![global]);
        assert_eq!(result.missing, vec![private.id]);
        assert_eq!(fs::read_dir(root.join("v2/consumers")).unwrap().count(), 0);
    }

    #[test]
    fn pagination_and_search_are_stable_and_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let v1 = ensure_layout(root).unwrap();
        let agent = register_agent(&v1, "sample", "one", "linux", root).unwrap();
        for i in 0..8 {
            publish_message(
                &v1,
                &agent.id,
                draft(&format!("Update {i}"), Vec::new(), Some("global".into())),
            )
            .unwrap();
        }
        let state = snapshot(root).unwrap();
        let mut q = HistoryQuery {
            limit: 3,
            ..Default::default()
        };
        let first = history(root, &state, &agent, &q).unwrap();
        assert_eq!(first.messages.len(), 3);
        q.before = first.next_cursor;
        let second = history(root, &state, &agent, &q).unwrap();
        assert_eq!(second.messages.len(), 3);
        assert!(second.messages[0].id < first.messages[2].id);
        let all = history(root, &state, &agent, &HistoryQuery::default()).unwrap();
        let forward = history(
            root,
            &state,
            &agent,
            &HistoryQuery {
                after: Some(all.messages[7].id.clone()),
                limit: 3,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(forward.messages[0].id < forward.messages[1].id);
        let search = history(
            root,
            &state,
            &agent,
            &HistoryQuery {
                search: Some("UPDATE 4".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(search.messages.len(), 1);
        assert_eq!(fs::read_dir(root.join("v2/consumers")).unwrap().count(), 0);
    }

    #[test]
    fn cursor_skips_old_partitions_and_filters_compose() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let v1 = ensure_layout(root).unwrap();
        let agent = register_agent(&v1, "sample", "one", "linux", root).unwrap();
        let message = publish_message(
            &v1,
            &agent.id,
            draft("release ready", Vec::new(), Some("global".into())),
        )
        .unwrap();
        // A corrupt old partition must not be opened by a forward query whose
        // cursor excludes that whole year.
        let old = Ulid::from_parts(1_000_000, 1).to_string();
        let path = message_id_path(root, "global", &old).unwrap();
        crate::storage::write_document(&path, &serde_json::json!({"bad":"record"})).unwrap();
        let state = snapshot(root).unwrap();
        let q = HistoryQuery {
            after: Some(Ulid::from_parts(2_000_000_000, 1).to_string()),
            sender: Some(agent.id.clone()),
            tags: vec!["release".into()],
            group: Some("global".into()),
            thread: Some(message.thread.clone()),
            search: Some("READY".into()),
            ..Default::default()
        };
        let found = history(root, &state, &agent, &q).unwrap();
        assert_eq!(found.messages, vec![message]);
        assert!(found.next_cursor.is_none());
        let q = HistoryQuery {
            tags: vec!["absent".into()],
            ..q
        };
        assert!(
            history(root, &state, &agent, &q)
                .unwrap()
                .messages
                .is_empty()
        );
        assert!(
            history(
                root,
                &state,
                &agent,
                &HistoryQuery {
                    limit: 0,
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert!(messages_get(root, &state, &agent, &["../invalid".into()]).is_err());
    }

    #[test]
    fn discovery_pages_filter_and_resume_without_repeating() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let v1 = ensure_layout(root).unwrap();
        let agent = register_agent(&v1, "sample", "one", "linux", root).unwrap();
        register_agent(&v1, "sample", "two", "linux", root).unwrap();
        crate::storage::touch_presence(&v1, &agent.id).unwrap();
        crate::storage::create_group(&v1, "release", &agent.id).unwrap();
        crate::storage::join_group(&v1, "release", &agent.id).unwrap();
        let state = snapshot(root).unwrap();
        let first = agents(
            &state,
            &AgentQuery {
                limit: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        let second = agents(
            &state,
            &AgentQuery {
                after: first.next_cursor,
                limit: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(first.agents.len(), 1);
        assert_eq!(second.agents.len(), 1);
        assert_ne!(first.agents[0].agent.id, second.agents[0].agent.id);
        assert_eq!(
            agents(
                &state,
                &AgentQuery {
                    online: Some(true),
                    ..Default::default()
                }
            )
            .unwrap()
            .agents
            .len(),
            1
        );
        let group_first = groups(
            &state,
            &agent,
            &GroupQuery {
                limit: Some(1),
                joined: true,
                ..Default::default()
            },
        )
        .unwrap();
        let group_second = groups(
            &state,
            &agent,
            &GroupQuery {
                limit: Some(1),
                joined: true,
                after: group_first.next_cursor,
                ..Default::default()
            },
        )
        .unwrap();
        assert_ne!(group_first.groups[0].name, group_second.groups[0].name);
    }
}

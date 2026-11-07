//! Board-wide helpers shared by the chat page, the wake listener and remote
//! access: human-readable agent names, mentions, hop counting, activity
//! status, and an operator view of every message on the board.
//!
//! The operator view reads every agent's sender route. Every published
//! message is written to `direct:<sender>` first, so the union of all sender
//! routes is the whole board without new indexes or format changes. This view
//! is for the person who owns the board; agent reads stay relevance-filtered.
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use chrono::Utc;
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ulid::Ulid;

use crate::model::{Agent, Message};
use crate::routed::{
    document_files_recursive, is_expired, newest_paths, prune, recent_paths, route_directory,
};
use crate::storage::{BoardState, normalize_session, read_document, write_document};

pub const ACTIVITY_SCHEMA: &str = "aiboard.activity.v1";
/// Agent-to-agent replies in a row before wake-ups pause for the founder.
pub const DEFAULT_HOP_LIMIT: u64 = 6;
/// `meta` key carrying the number of agent replies since a human last spoke.
pub const HOPS_KEY: &str = "zboard_hops";
/// `meta` key marking a message written by a person rather than an agent.
pub const HUMAN_KEY: &str = "human";

/// The name people should see. Sessions registered with a role name keep it
/// (`coo`, `brand-marketer`); generated session IDs fall back to the agent ID.
pub fn display_name(agent: &Agent) -> String {
    if looks_generated(&agent.session_id) {
        agent.id.clone()
    } else {
        agent.session_id.clone()
    }
}

fn looks_generated(session: &str) -> bool {
    let uuid = session.len() == 36
        && session.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        });
    let ulid = session.len() == 26 && session.parse::<Ulid>().is_ok();
    uuid || ulid || session.starts_with("mcp-")
}

/// Validate a role name used as a stable session, e.g. `brand-marketer`.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        || !name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
    {
        bail!("agent names are 1-64 lowercase letters, numbers, '-' or '_', starting with a letter");
    }
    Ok(())
}

/// The session ID to register for a role name. If the project already has an
/// agent whose session normalises to the same ID (say `Brand Marketer` for
/// `brand-marketer`), reuse it so the name maps to that existing agent.
pub fn session_for_name(root: &Path, project: &str, name: &str) -> String {
    let Ok(key) = normalize_session(name) else {
        return name.to_owned();
    };
    let mut state = BoardState::default();
    crate::storage::scan_metadata(&root.join("v1"), &mut state);
    state
        .agents
        .get(&format!("{project}-{key}"))
        .map(|agent| agent.session_id.clone())
        .unwrap_or_else(|| name.to_owned())
}

/// Agents named with `@name` in a message body. Matching ignores case and
/// punctuation, so `@brand-marketer`, `@BrandMarketer` and the full agent ID
/// all resolve to the same registration.
pub fn mentioned_agents(state: &BoardState, body: &str) -> Vec<String> {
    let mut tokens = HashSet::new();
    let mut chars = body.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        if character != '@' {
            continue;
        }
        let preceded_by_word = body[..index]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        if preceded_by_word {
            continue; // an email address, not a mention
        }
        let mut token = String::new();
        while let Some((_, next)) = chars.peek() {
            if next.is_ascii_alphanumeric() || matches!(next, '-' | '_') {
                token.push(*next);
                chars.next();
            } else {
                break;
            }
        }
        if let Ok(key) = normalize_session(&token) {
            tokens.insert(key);
        }
    }
    if tokens.is_empty() {
        return Vec::new();
    }
    let mut matches: Vec<String> = state
        .agents
        .values()
        .filter(|agent| {
            normalize_session(&agent.session_id).is_ok_and(|key| tokens.contains(&key))
                || normalize_session(&agent.id).is_ok_and(|key| tokens.contains(&key))
        })
        .map(|agent| agent.id.clone())
        .collect();
    matches.sort();
    matches.dedup();
    matches
}

pub fn is_human(message: &Message) -> bool {
    message
        .meta
        .as_ref()
        .and_then(|meta| meta.get(HUMAN_KEY))
        .and_then(Value::as_bool)
        == Some(true)
}

pub fn hops(message: &Message) -> u64 {
    message
        .meta
        .as_ref()
        .and_then(|meta| meta.get(HOPS_KEY))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Hop count for a new message: zero from a person or a new conversation,
/// otherwise one more than the message it answers.
pub fn next_hops(parent: Option<&Message>, human: bool) -> u64 {
    match parent {
        _ if human => 0,
        Some(parent) if is_human(parent) => 1,
        Some(parent) => hops(parent) + 1,
        None => 0,
    }
}

/// Record the hop count in `meta` when it is an object or absent. Callers that
/// put a non-object in `meta` opt out of hop tracking for that message.
pub fn stamp_hops(meta: Option<Value>, hops: u64) -> Option<Value> {
    match meta {
        None if hops == 0 => None,
        None => Some(serde_json::json!({ HOPS_KEY: hops })),
        Some(Value::Object(mut map)) => {
            if hops > 0 && !map.contains_key(HOPS_KEY) {
                map.insert(HOPS_KEY.into(), hops.into());
            }
            Some(Value::Object(map))
        }
        other => other,
    }
}

pub fn hop_limit() -> u64 {
    std::env::var("ZBOARD_HOP_LIMIT")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(DEFAULT_HOP_LIMIT)
}

/// Whether a message should still wake agents under the loop guard. People
/// always get through.
pub fn within_hop_limit(message: &Message) -> bool {
    is_human(message) || hops(message) < hop_limit()
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Eq, Serialize)]
pub struct Activity {
    pub schema: String,
    pub id: String,
    pub agent: String,
    /// `working`, `idle` or `offline`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub at: String,
}

fn activity_directory(root: &Path, agent: &str) -> Result<PathBuf> {
    if agent.is_empty()
        || !agent
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("invalid agent ID");
    }
    Ok(root.join("v2/activity").join(agent))
}

/// Publish what an agent is doing now. Stored like presence: small immutable
/// documents under `v2/activity/<agent>/`, newest wins, four kept.
pub fn set_activity(root: &Path, agent: &str, state: &str, detail: Option<String>) -> Result<Activity> {
    if !matches!(state, "working" | "idle" | "offline") {
        bail!("activity must be working, idle or offline");
    }
    let detail = detail.map(|text| text.chars().take(200).collect::<String>());
    let directory = activity_directory(root, agent)?;
    let id = Ulid::new().to_string();
    let activity = Activity {
        schema: ACTIVITY_SCHEMA.into(),
        id: id.clone(),
        agent: agent.into(),
        state: state.into(),
        detail,
        at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    };
    write_document(&directory.join(format!("{id}.json.zst")), &activity)?;
    prune(&directory, 4);
    Ok(activity)
}

pub fn activity(root: &Path, agent: &str) -> Option<Activity> {
    let directory = activity_directory(root, agent).ok()?;
    let mut paths = document_files_recursive(&directory);
    paths.sort();
    paths.into_iter().rev().find_map(|path| {
        read_document::<Activity>(&path)
            .ok()
            .filter(|a| a.schema == ACTIVITY_SCHEMA && a.agent == agent)
    })
}

/// Every message on the board, loaded from each agent's sender route.
/// Documents are immutable, so a path read once never needs reading again.
#[derive(Default)]
pub struct BoardView {
    pub messages: BTreeMap<String, Message>,
    /// Arrival order, so pollers see late-visible messages with older IDs.
    pub arrivals: Vec<String>,
    loaded: HashSet<PathBuf>,
    loaded_agents: HashSet<String>,
}

impl BoardView {
    /// Load up to `per_agent` recent messages for agents not yet loaded, then
    /// pick up anything new in the last ten minutes for everyone.
    pub fn refresh(&mut self, root: &Path, agents: impl Iterator<Item = String>, per_agent: usize) -> Vec<String> {
        let mut paths = Vec::new();
        for agent in agents {
            let route = format!("direct:{agent}");
            if self.loaded_agents.insert(agent) {
                paths.extend(newest_paths(&route_directory(root, &route), per_agent));
            }
            paths.extend(recent_paths(root, &route));
        }
        self.read_paths(paths)
    }

    fn read_paths(&mut self, paths: Vec<PathBuf>) -> Vec<String> {
        let mut new = Vec::new();
        for path in paths {
            if self.loaded.contains(&path) {
                continue;
            }
            let Ok(message) = read_document::<Message>(&path) else {
                continue; // retried on the next refresh, like other readers
            };
            self.loaded.insert(path);
            if is_expired(&message) || self.messages.contains_key(&message.id) {
                continue;
            }
            new.push(message.id.clone());
            self.messages.insert(message.id.clone(), message);
        }
        new.sort();
        self.arrivals.extend(new.iter().cloned());
        new
    }

    /// A whole conversation, reading older history from disk if needed.
    pub fn thread(&mut self, root: &Path, agents: &[String], thread: &str) -> Vec<Message> {
        let cached_root = self.messages.contains_key(thread);
        if !cached_root {
            let mut paths = Vec::new();
            for agent in agents {
                paths.extend(document_files_recursive(&route_directory(
                    root,
                    &format!("direct:{agent}"),
                )));
            }
            self.read_paths(paths);
        }
        self.messages
            .values()
            .filter(|message| message.thread == thread)
            .cloned()
            .collect()
    }

    /// Most recent message sent by each agent.
    pub fn last_by_sender(&self) -> BTreeMap<String, &Message> {
        let mut last = BTreeMap::new();
        for message in self.messages.values() {
            last.insert(message.from.clone(), message);
        }
        last
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{MessageDraft, ensure_layout, publish_message, register_agent, scan_metadata};
    use serde_json::json;

    fn agent(session: &str) -> Agent {
        Agent {
            schema: crate::model::AGENT_SCHEMA.into(),
            id: format!("demo-{}", normalize_session(session).unwrap()),
            project: "demo".into(),
            session_id: session.into(),
            platform: "macos".into(),
            path: "/demo".into(),
            registered_at: String::new(),
        }
    }

    #[test]
    fn role_names_display_and_generated_ids_fall_back() {
        assert_eq!(display_name(&agent("brand-marketer")), "brand-marketer");
        assert_eq!(
            display_name(&agent("mcp-01M6TCM2J4PXW4FBR0CXRPEAEH")),
            "demo-mcp01m6tcm2j4pxw4fbr0cxrpeaeh"
        );
        assert_eq!(
            display_name(&agent("019923b8-c112-7c83-a486-1c977ea93ff1")),
            "demo-019923b8c1127c83a4861c977ea93ff1"
        );
        assert!(validate_name("brand-marketer").is_ok());
        assert!(validate_name("Brand").is_err());
        assert!(validate_name("1coo").is_err());
        assert!(validate_name("../x").is_err());
    }

    #[test]
    fn role_names_adopt_an_existing_registration_with_the_same_id() {
        let dir = tempfile::tempdir().unwrap();
        let v1 = ensure_layout(dir.path()).unwrap();
        register_agent(&v1, "repeat", "Brand Marketer", "macos", dir.path()).unwrap();
        assert_eq!(session_for_name(dir.path(), "repeat", "brand-marketer"), "Brand Marketer");
        assert_eq!(session_for_name(dir.path(), "repeat", "coo"), "coo");
        assert_eq!(session_for_name(dir.path(), "other", "brand-marketer"), "brand-marketer");
    }

    #[test]
    fn mentions_resolve_names_ids_and_ignore_emails() {
        let mut state = BoardState::default();
        for session in ["coo", "brand-marketer", "sales"] {
            let a = agent(session);
            state.agents.insert(a.id.clone(), a);
        }
        assert_eq!(
            mentioned_agents(&state, "@coo and @BrandMarketer, see sales@example.com"),
            ["demo-brandmarketer", "demo-coo"]
        );
        assert_eq!(mentioned_agents(&state, "@demo-sales please"), ["demo-sales"]);
        assert!(mentioned_agents(&state, "@nobody").is_empty());
    }

    #[test]
    fn hops_grow_between_agents_and_reset_for_people() {
        let make = |meta: Option<Value>| Message {
            schema: crate::model::MESSAGE_SCHEMA.into(),
            id: Ulid::new().to_string(),
            timestamp: String::new(),
            from: "a".into(),
            to: vec!["b".into()],
            group: None,
            thread: "t".into(),
            reply_to: None,
            message: "x".into(),
            tags: vec![],
            meta,
            expires_at: None,
        };
        let person = make(Some(json!({"human": true})));
        assert_eq!(next_hops(Some(&person), false), 1);
        let agent_reply = make(stamp_hops(None, 1));
        assert_eq!(hops(&agent_reply), 1);
        assert_eq!(next_hops(Some(&agent_reply), false), 2);
        assert_eq!(next_hops(Some(&agent_reply), true), 0);
        assert_eq!(next_hops(None, false), 0);
        assert!(within_hop_limit(&make(stamp_hops(None, DEFAULT_HOP_LIMIT - 1))));
        assert!(!within_hop_limit(&make(stamp_hops(None, DEFAULT_HOP_LIMIT))));
        let mut loud_person = make(Some(json!({"human": true, HOPS_KEY: 99})));
        assert!(within_hop_limit(&loud_person));
        loud_person.meta = Some(json!(["not", "an", "object"]));
        assert_eq!(stamp_hops(loud_person.meta.clone(), 3), loud_person.meta);
    }

    #[test]
    fn activity_newest_wins_and_rejects_bad_input() {
        let dir = tempfile::tempdir().unwrap();
        set_activity(dir.path(), "demo-coo", "working", Some("reviewing".into())).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        set_activity(dir.path(), "demo-coo", "idle", None).unwrap();
        assert_eq!(activity(dir.path(), "demo-coo").unwrap().state, "idle");
        assert!(set_activity(dir.path(), "demo-coo", "asleep", None).is_err());
        assert!(set_activity(dir.path(), "../x", "idle", None).is_err());
        assert!(activity(dir.path(), "demo-none").is_none());
    }

    #[test]
    fn board_view_sees_direct_messages_between_other_agents() {
        let dir = tempfile::tempdir().unwrap();
        let v1 = ensure_layout(dir.path()).unwrap();
        let a = register_agent(&v1, "demo", "coo", "macos", dir.path()).unwrap();
        let b = register_agent(&v1, "demo", "sales", "macos", dir.path()).unwrap();
        let draft = |to: Vec<String>, group: Option<String>, body: &str| MessageDraft {
            to,
            group,
            thread: None,
            reply_to: None,
            body: body.into(),
            tags: vec![],
            meta: None,
            ttl_seconds: None,
        };
        let dm = publish_message(&v1, &a.id, draft(vec![b.id.clone()], None, "private")).unwrap();
        let team = publish_message(&v1, &b.id, draft(vec![], Some("global".into()), "hello")).unwrap();
        let mut state = BoardState::default();
        scan_metadata(&v1, &mut state);
        let mut view = BoardView::default();
        let new = view.refresh(dir.path(), state.agents.keys().cloned(), 100);
        assert_eq!(new.len(), 2);
        assert!(view.messages.contains_key(&dm.id) && view.messages.contains_key(&team.id));
        assert!(view.refresh(dir.path(), state.agents.keys().cloned(), 100).is_empty());
        let agents: Vec<_> = state.agents.keys().cloned().collect();
        assert_eq!(view.thread(dir.path(), &agents, &dm.thread).len(), 1);
        assert_eq!(view.last_by_sender()[&b.id].id, team.id);
    }
}

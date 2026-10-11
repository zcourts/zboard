//! Request/response operations that never own a listener's delivery checkpoint.
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ulid::Ulid;

use crate::model::{Agent, Message};
use crate::storage::{self, BoardState, MessageDraft};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SendRequest {
    #[serde(default)]
    pub to: Vec<String>,
    pub group: Option<String>,
    pub message: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub meta: Option<Value>,
    pub ttl_seconds: Option<u64>,
    pub idempotency_key: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ReplyRequest {
    pub message_id: String,
    pub message: String,
    pub tags: Option<Vec<String>>,
    pub meta: Option<Value>,
    pub ttl_seconds: Option<u64>,
    pub idempotency_key: Option<String>,
}

pub struct Client {
    pub root: PathBuf,
    pub agent: Agent,
    pub state: BoardState,
}

impl Client {
    pub fn read_only(root: &Path, project: &str, session: &str, _path: &Path) -> Result<Self> {
        let state = crate::query::snapshot(root)?;
        let id = format!("{project}-{}", storage::normalize_session(session)?);
        let agent = state
            .agents
            .get(&id)
            .cloned()
            .context("identity not registered; run zboard identity first")?;
        if agent.session_id != session {
            bail!("session identity mismatch");
        }
        Ok(Self {
            root: root.to_path_buf(),
            agent,
            state,
        })
    }

    pub fn open(root: &Path, project: &str, session: &str, path: &Path) -> Result<Self> {
        let version_root = storage::ensure_layout(root)?;
        let agent =
            storage::register_agent(&version_root, project, session, std::env::consts::OS, path)?;
        storage::touch_presence(&version_root, &agent.id)?;
        let state = crate::query::snapshot(root)?;
        Ok(Self {
            root: root.to_path_buf(),
            agent,
            state,
        })
    }

    pub fn send(&mut self, request: SendRequest) -> Result<Message> {
        self.publish(request, None, None)
    }

    pub fn reply(&mut self, request: ReplyRequest) -> Result<Message> {
        let lookup = crate::query::messages_get(
            &self.root,
            &self.state,
            &self.agent,
            std::slice::from_ref(&request.message_id),
        )?;
        let parent = lookup
            .messages
            .into_iter()
            .next()
            .context("message not found or not relevant")?;
        let recipients = if parent.group.is_some() {
            Vec::new()
        } else {
            storage::direct_reply_recipients(&parent, &self.agent.id)
        };
        self.publish(
            SendRequest {
                to: recipients,
                group: parent.group,
                message: request.message,
                tags: request.tags.unwrap_or(parent.tags),
                meta: request.meta,
                ttl_seconds: request.ttl_seconds,
                idempotency_key: request.idempotency_key,
            },
            Some(parent.thread),
            Some(parent.id),
        )
    }

    fn publish(
        &mut self,
        mut request: SendRequest,
        thread: Option<String>,
        reply_to: Option<String>,
    ) -> Result<Message> {
        let id = request
            .idempotency_key
            .take()
            .unwrap_or_else(|| Ulid::new().to_string());
        let result = (|| {
            let id: Ulid = id.parse().context("idempotency key must be a ULID")?;
            let id = id.to_string();
            request.to.sort();
            request.to.dedup();
            if let Some(group) = &request.group {
                if !request.to.is_empty() {
                    bail!("provide recipients or a group, not both");
                }
                if group != "global"
                    && group != &format!("project:{}", self.agent.project)
                    && !self
                        .state
                        .memberships
                        .contains(&(group.clone(), self.agent.id.clone()))
                {
                    bail!("join group '{group}' before sending");
                }
            } else {
                if request.to.is_empty() {
                    bail!("provide a recipient or group");
                }
                for recipient in &request.to {
                    if !self.state.agents.contains_key(recipient) {
                        bail!("unknown recipient '{recipient}'");
                    }
                }
            }
            if serde_json::to_vec(&request)?.len() > 900_000 {
                bail!("message exceeds 900000-byte request limit; split the payload");
            }
            // This lock is host-local, never on the shared/WebDAV filesystem.
            // Different hosts must not concurrently reuse one identity/retry key.
            let lock_path = local_state_root()?
                .join("request-locks")
                .join(format!("{}.lock", self.agent.id));
            fs::create_dir_all(lock_path.parent().unwrap())?;
            let lock = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&lock_path)?;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match lock.try_lock() {
                    Ok(()) => break,
                    Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20))
                    }
                    Err(error) => bail!("request lock unavailable: {error}"),
                }
            }
            let message = storage::publish_message_with_id(
                &self.root.join("v1"),
                &self.agent.id,
                MessageDraft {
                    to: request.to,
                    group: request.group,
                    thread,
                    reply_to,
                    body: request.message,
                    tags: request.tags,
                    meta: request.meta,
                    ttl_seconds: request.ttl_seconds,
                },
                Some(&id),
            )?;
            Ok(message)
        })();
        result.with_context(|| format!("request_id={id}; retry the same payload with this idempotency key after fixing the error"))
    }

    pub fn status(&self) -> Result<Value> {
        let path =
            local_state_root()?.join(format!("{}-{}", self.agent.project, self.agent.session_id));
        let checkpoint = path.join("wake.json");
        let events = path.join("events.jsonl");
        let wake: Option<Value> = match fs::read(&checkpoint) {
            Ok(bytes) => Some(serde_json::from_slice(&bytes).context("invalid wake checkpoint")?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let lock_held = match File::open(path.join("wake.lock")) {
            Ok(file) => match file.try_lock() {
                Ok(()) => Some(false),
                Err(std::fs::TryLockError::WouldBlock) => Some(true),
                Err(_) => None,
            },
            Err(_) => None,
        };
        let bytes = fs::metadata(&events).ok().map(|m| m.len());
        let offset = wake
            .as_ref()
            .and_then(|v| v.get("offset"))
            .and_then(Value::as_u64);
        Ok(json!({"agent": self.agent, "board_root": self.root,
            "listener_checkpoint_untouched": true,
            "wake": {"configured": wake.is_some(), "lock_held": lock_held,
                "state_directory": path, "event_bytes": bytes, "queued_offset": offset,
                "pending_bytes": bytes.zip(offset).map(|(size,offset)| size.saturating_sub(offset))},
            "presence": storage::agent_statuses(&self.state).into_iter().find(|a| a.agent.id == self.agent.id)}))
    }

    pub fn doctor(&self) -> Result<Value> {
        let status = self.status()?;
        let mut issues = Vec::new();
        if !self.root.join("v2").is_dir() {
            issues.push("board v2 directory is missing");
        }
        if status["wake"]["configured"] == true && status["wake"]["lock_held"] != true {
            issues.push("wake checkpoint exists but bridge lock is not held; start your own configured bridge");
        }
        if let (Some(bytes), Some(offset)) = (
            status["wake"]["event_bytes"].as_u64(),
            status["wake"]["queued_offset"].as_u64(),
        ) {
            if offset > bytes {
                issues.push("wake event log was truncated; restore it rather than silently resetting delivery");
            }
        }
        let directory =
            local_state_root()?.join(format!("{}-{}", self.agent.project, self.agent.session_id));
        let mut logs = serde_json::Map::new();
        for name in ["wake.log", "board-errors.log"] {
            if let Ok(mut file) = File::open(directory.join(name)) {
                use std::io::{Read, Seek, SeekFrom};
                let len = file.metadata()?.len();
                file.seek(SeekFrom::Start(len.saturating_sub(4096)))?;
                let mut tail = String::new();
                file.read_to_string(&mut tail)?;
                logs.insert(name.to_owned(), Value::String(tail));
            }
        }
        Ok(
            json!({"status":status,"healthy":issues.is_empty(),"issues":issues,"recent_diagnostics":logs,
            "note":"Read-only checks; does not restart services, send presence pings, repair data or test free disk space by writing."}),
        )
    }
}

pub fn local_state_root() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(path).join("zboard"));
    }
    if let Some(path) = std::env::var_os("LOCALAPPDATA").filter(|_| cfg!(windows)) {
        return Ok(PathBuf::from(path).join("zboard"));
    }
    let home =
        std::env::var_os("HOME").context("set XDG_STATE_HOME or HOME for local Zboard state")?;
    Ok(PathBuf::from(home).join(".local/state/zboard"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_repairs_partial_delivery_and_rejects_changed_destination() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut sender = Client::open(root, "sender", "client-retry", root).unwrap();
        let receiver = Client::open(root, "receiver", "client-receiver", root).unwrap();
        sender.state = crate::query::snapshot(root).unwrap();
        let id = Ulid::new().to_string();
        let request = SendRequest {
            to: vec![receiver.agent.id.clone()],
            message: "payload".into(),
            idempotency_key: Some(id.clone()),
            ..Default::default()
        };
        let first = sender.send(request.clone()).unwrap();
        let path =
            crate::routed::message_id_path(root, &format!("direct:{}", receiver.agent.id), &id)
                .unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(sender.send(request.clone()).unwrap(), first);
        assert!(path.exists());
        let changed = SendRequest {
            to: vec![],
            group: Some("global".into()),
            ..request
        };
        assert!(sender.send(changed).is_err());
        assert!(
            !crate::routed::message_id_path(root, "global", &id)
                .unwrap()
                .exists()
        );
        assert!(
            fs::read_dir(root.join("v2/consumers"))
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[test]
    fn publication_failure_reports_retry_key_not_success() {
        let dir = tempfile::tempdir().unwrap();
        let mut client = Client::open(dir.path(), "sender", "failure", dir.path()).unwrap();
        let id = Ulid::new().to_string();
        let directory =
            crate::routed::route_directory(dir.path(), &format!("direct:{}", client.agent.id));
        fs::create_dir_all(directory.parent().unwrap()).unwrap();
        fs::write(&directory, "obstruction").unwrap();
        let error = client
            .send(SendRequest {
                group: Some("global".into()),
                message: "test".into(),
                idempotency_key: Some(id.clone()),
                ..Default::default()
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains(&format!("request_id={id}")));
        assert!(
            !crate::routed::message_id_path(dir.path(), "global", &id)
                .unwrap()
                .exists()
        );
    }
}

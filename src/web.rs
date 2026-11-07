//! `zboard web`: a chat page for the person who owns the board.
//!
//! Serves one embedded HTML page and a small JSON API on localhost. The
//! person posts as a registered agent (default `founder`) whose messages
//! carry `meta.human: true`, which resets the agent loop guard. The page
//! reads the whole board, including agent-to-agent direct messages: on a
//! board one person owns, filtering is for relevance, not access.
use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::board::{self, BoardView};
use crate::client::{Client, ReplyRequest, SendRequest};
use crate::http::{self, Request, Response};
use crate::model::{Agent, Message};
use crate::storage::{self, BoardState};

const PAGE: &str = include_str!("web/index.html");
const INITIAL_MESSAGES: usize = 1500;

pub struct WebOptions {
    pub root: PathBuf,
    pub project: String,
    pub person: String,
    pub team: String,
    pub listen: SocketAddr,
}

struct Web {
    root: PathBuf,
    team: String,
    person: Agent,
    path: PathBuf,
    view: BoardView,
    metadata: BoardState,
    refreshed: Option<Instant>,
    presence: Option<Instant>,
    dismissed: BTreeSet<String>,
    dismissed_path: PathBuf,
}

pub fn command(mut args: impl Iterator<Item = String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let mut root = std::env::var_os("ZBOARD_ROOT").map(PathBuf::from);
    let mut project = std::env::var("ZBOARD_PROJECT").ok();
    let mut person = "founder".to_owned();
    let mut team = "team".to_owned();
    let mut host: IpAddr = "127.0.0.1".parse()?;
    let mut port: u16 = 7777;
    while let Some(flag) = args.next() {
        let mut value = || crate::app::required_value(&mut args, &flag);
        match flag.as_str() {
            "--root" => root = Some(PathBuf::from(value()?)),
            "--project" => project = Some(value()?),
            "--as" => person = value()?,
            "--team" => team = value()?,
            "--host" => host = value()?.parse().context("--host must be an IP address")?,
            "--port" => port = value()?.parse().context("--port must be a number")?,
            "--help" | "-h" => {
                println!("zboard web [--root PATH] [--project SLUG] [--as NAME] [--team GROUP] [--port 7777]\n\nServes the team chat on http://127.0.0.1:PORT. Only loopback addresses are allowed:\nthe page has no login, so reach it from another machine through an SSH tunnel or VPN-forwarded port.");
                return Ok(());
            }
            other => bail!("unknown web option '{other}'"),
        }
    }
    if !host.is_loopback() {
        bail!("zboard web only listens on loopback (127.0.0.1 or ::1) because the page has no login; use an SSH tunnel to reach it remotely");
    }
    board::validate_name(&person)?;
    storage::validate_slug(&team, "group")?;
    let project = project.unwrap_or_else(|| crate::project_slug(&cwd));
    run(WebOptions {
        root: root.unwrap_or_else(|| crate::app::default_root(&cwd)),
        project,
        person,
        team,
        listen: SocketAddr::new(host, port),
    })
}

pub fn run(options: WebOptions) -> Result<()> {
    let client = Client::open(&options.root, &options.project, &options.person, &options.root)?;
    let version_root = options.root.join("v1");
    storage::create_group(&version_root, &options.team, &client.agent.id)?;
    storage::join_group(&version_root, &options.team, &client.agent.id)?;
    let dismissed_path = local_directory(&options.root)?.join(format!("{}-dismissed.json", client.agent.id));
    let dismissed = std::fs::read(&dismissed_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let listener = TcpListener::bind(options.listen).with_context(|| format!("listen on {}", options.listen))?;
    let address = listener.local_addr()?;
    let web = Arc::new(Mutex::new(Web {
        root: options.root.clone(),
        team: options.team,
        person: client.agent,
        path: options.root,
        view: BoardView::default(),
        metadata: BoardState::default(),
        refreshed: None,
        presence: None,
        dismissed,
        dismissed_path,
    }));
    web.lock().unwrap().refresh();
    println!("{}", json!({"type": "ready", "url": format!("http://{address}/")}));
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let allowed_hosts = Arc::new(allowed_hosts(address));
    http::serve(
        listener,
        // Screenshots are uploaded as raw image bodies (up to 10 MB).
        crate::files::MAX_BYTES as usize + 64 * 1024,
        Arc::new(move |request| route(&web, &allowed_hosts, request)),
    )
}

fn allowed_hosts(address: SocketAddr) -> Vec<String> {
    let port = address.port();
    let mut hosts = vec![format!("localhost:{port}"), format!("127.0.0.1:{port}"), format!("[::1]:{port}")];
    hosts.push(address.to_string());
    hosts
}

fn local_directory(root: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(root)?;
    let digest: String = Sha256::digest(canonical.to_string_lossy().as_bytes())
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect();
    let directory = crate::client::local_state_root()?.join("web").join(digest);
    crate::notify::private_dir(&directory)?;
    Ok(directory)
}

fn route(web: &Arc<Mutex<Web>>, allowed_hosts: &[String], request: Request) -> Response {
    // DNS rebinding guard: only answer requests addressed to this machine.
    if !request
        .header("host")
        .is_some_and(|host| allowed_hosts.iter().any(|allowed| allowed == host))
    {
        return Response::error(403, "unexpected Host header");
    }
    if request.method == "POST" && request.header("x-zboard") != Some("1") {
        // Browsers cannot add this header cross-site without a preflight we
        // never approve, so other web pages cannot post as the founder.
        return Response::error(403, "missing X-Zboard header");
    }
    let mut web = web.lock().unwrap();
    let result = match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => return Response::html(PAGE),
        ("GET", path) if path.starts_with("/files/") => {
            return match crate::files::read(&web.root, &path["/files/".len()..]) {
                Some((bytes, kind)) => Response::file(kind, bytes),
                None => Response::error(404, "not found"),
            }
        }
        ("GET", "/api/state") => web.state(&request),
        ("GET", "/api/thread") => web.thread(&request),
        ("POST", "/api/upload") => web.upload(&request),
        ("POST", "/api/send") => request.json().and_then(|body| web.send(&body)),
        ("POST", "/api/answer") => request.json().and_then(|body| web.answer(&body)),
        ("POST", "/api/continue") => request.json().and_then(|body| web.carry_on(&body)),
        ("POST", "/api/job") => request.json().and_then(|body| web.job(&body)),
        ("POST", "/api/rule") => request.json().and_then(|body| web.rule(&body)),
        ("POST", "/api/dismiss") => request.json().and_then(|body| web.dismiss(&body)),
        _ => return Response::error(404, "not found"),
    };
    match result {
        Ok(value) => Response::json(200, &json!({"ok": true, "data": value})),
        Err(error) => Response::json(400, &json!({"ok": false, "error": format!("{error:#}")})),
    }
}

fn text(body: &Value, key: &str) -> Result<String> {
    let value = body[key].as_str().with_context(|| format!("{key} is required"))?.trim().to_owned();
    if value.is_empty() {
        bail!("{key} must not be empty");
    }
    if value.len() > 64 * 1024 {
        bail!("{key} is too long");
    }
    Ok(value)
}

/// `#tag` words in a message become tags.
fn hash_tags(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|word| word.strip_prefix('#'))
        .map(|tag| {
            tag.trim_end_matches(|c: char| !c.is_ascii_alphanumeric())
                .to_ascii_lowercase()
        })
        .filter(|tag| storage::normalize_tags(vec![tag.clone()]).is_ok())
        .collect()
}

impl Web {
    fn refresh(&mut self) {
        if self.refreshed.is_some_and(|at| at.elapsed() < Duration::from_millis(900)) {
            return;
        }
        let mut metadata = BoardState::default();
        storage::scan_metadata(&self.root.join("v1"), &mut metadata);
        self.metadata = metadata;
        let agents: Vec<String> = self.metadata.agents.keys().cloned().collect();
        self.view.refresh(&self.root, agents.into_iter(), 400);
        self.refreshed = Some(Instant::now());
        // The page being open means the person is around.
        if self.presence.is_none_or(|at| at.elapsed() > Duration::from_secs(20)) {
            let _ = storage::touch_presence(&self.root.join("v1"), &self.person.id);
            self.presence = Some(Instant::now());
        }
    }

    fn client(&self) -> Result<Client> {
        Client::open(&self.path, &self.person.project, &self.person.session_id, &self.path)
    }

    fn human_meta(extra: Value) -> Value {
        let mut meta = json!({ board::HUMAN_KEY: true });
        if let (Some(map), Value::Object(extra)) = (meta.as_object_mut(), extra) {
            map.extend(extra);
        }
        meta
    }

    fn state(&mut self, request: &Request) -> Result<Value> {
        self.refresh();
        let cursor: usize = request
            .query
            .get("cursor")
            .and_then(|c| c.parse().ok())
            .unwrap_or(0)
            .min(self.view.arrivals.len());
        let start = if cursor == 0 {
            self.view.arrivals.len().saturating_sub(INITIAL_MESSAGES)
        } else {
            cursor
        };
        let messages: Vec<&Message> = self.view.arrivals[start..]
            .iter()
            .filter_map(|id| self.view.messages.get(id))
            .collect();
        let last = self.view.last_by_sender();
        let humans: BTreeSet<&str> = self
            .view
            .messages
            .values()
            .filter(|m| board::is_human(m))
            .map(|m| m.from.as_str())
            .collect();
        let statuses = storage::agent_statuses(&self.metadata);
        let agents: Vec<Value> = statuses
            .iter()
            .map(|status| {
                let agent = &status.agent;
                let activity = board::activity(&self.root, &agent.id);
                let running = crate::notify::is_running(&self.root, &agent.id);
                json!({
                    "id": agent.id,
                    "name": board::display_name(agent),
                    "project": agent.project,
                    "human": agent.id == self.person.id || humans.contains(agent.id.as_str()),
                    "online": status.online,
                    "last_seen": status.last_seen,
                    "activity": activity,
                    "running": running,
                    "last": last.get(&agent.id).map(|m| json!({
                        "id": m.id, "text": m.message.chars().take(200).collect::<String>(),
                        "timestamp": m.timestamp, "group": m.group, "to": m.to})),
                })
            })
            .collect();
        let groups: Vec<&String> = self.metadata.groups.keys().collect();
        Ok(json!({
            "me": {"id": self.person.id, "name": board::display_name(&self.person), "project": self.person.project},
            "team": self.team,
            "hop_limit": board::hop_limit(),
            "cursor": self.view.arrivals.len(),
            "reset": cursor == 0,
            "messages": messages,
            "agents": agents,
            "groups": groups,
            "dismissed": self.dismissed,
            "tags": crate::routed::tags(&self.root).unwrap_or_default(),
        }))
    }

    fn thread(&mut self, request: &Request) -> Result<Value> {
        let id = request.query.get("id").context("id is required")?;
        let _: ulid::Ulid = id.parse().context("invalid message ID")?;
        self.refresh();
        let thread = match self.view.messages.get(id) {
            Some(message) => message.thread.clone(),
            None => id.clone(),
        };
        let agents: Vec<String> = self.metadata.agents.keys().cloned().collect();
        let messages = self.view.thread(&self.root, &agents, &thread);
        Ok(json!({"thread": thread, "messages": messages}))
    }

    fn parent(&mut self, id: &str) -> Result<Message> {
        let _: ulid::Ulid = id.parse().context("invalid message ID")?;
        if !self.view.messages.contains_key(id) {
            self.refreshed = None;
            self.refresh();
        }
        if let Some(message) = self.view.messages.get(id) {
            return Ok(message.clone());
        }
        bail!("message {id} is not on this board")
    }

    /// Join a custom group so the person can post in it.
    fn ensure_member(&self, group: &str) -> Result<()> {
        if group == "global" || group.starts_with("project:") {
            return Ok(());
        }
        storage::join_group(&self.root.join("v1"), group, &self.person.id)?;
        Ok(())
    }

    fn reply(&mut self, parent_id: &str, body: String, tags: Option<Vec<String>>, meta: Value) -> Result<Value> {
        let parent = self.parent(parent_id)?;
        if let Some(group) = &parent.group {
            self.ensure_member(group)?;
        }
        let mut client = self.client()?;
        let message = client.reply_to_message(
            parent,
            ReplyRequest {
                message_id: parent_id.to_owned(),
                message: body,
                tags,
                meta: Some(Self::human_meta(meta)),
                ttl_seconds: None,
                idempotency_key: None,
            },
        )?;
        self.refreshed = None;
        Ok(json!({"message": message, "notifications": client.notifications}))
    }

    /// A screenshot pasted, dropped or picked in the chat page. The body is
    /// the raw image; it is stored now and attached when the message is sent.
    fn upload(&mut self, request: &Request) -> Result<Value> {
        let name = request.query.get("name").cloned().unwrap_or_default();
        crate::files::store_bytes(&self.root, &request.body, &name)
    }

    /// The person's message metadata, with any uploaded attachments.
    fn person_meta(&self, body: &Value, extra: Value) -> Result<Value> {
        let meta = Self::human_meta(extra);
        match body["attachments"].as_array() {
            Some(claimed) if !claimed.is_empty() => {
                crate::files::with_attachments(Some(meta), crate::files::verify(&self.root, claimed)?)
            }
            _ => Ok(meta),
        }
    }

    fn send(&mut self, body: &Value) -> Result<Value> {
        let has_files = body["attachments"].as_array().is_some_and(|a| !a.is_empty());
        let message = match text(body, "text") {
            // A screenshot on its own is a fine message.
            Err(_) if has_files => "(image)".to_owned(),
            other => other?,
        };
        let mut tags: Vec<String> = body["tags"]
            .as_array()
            .map(|tags| tags.iter().filter_map(|t| t.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        tags.extend(hash_tags(&message));
        if let Some(parent) = body["reply_to"].as_str() {
            let tags = (!tags.is_empty()).then_some(tags);
            let meta = self.person_meta(body, json!({}))?;
            return self.reply(parent, message, tags, meta);
        }
        let to: Vec<String> = body["to"]
            .as_array()
            .map(|to| to.iter().filter_map(|t| t.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        let group = if to.is_empty() {
            Some(body["group"].as_str().unwrap_or(&self.team).to_owned())
        } else {
            None
        };
        if let Some(group) = &group {
            self.ensure_member(group)?;
        }
        let meta = self.person_meta(body, json!({}))?;
        let mut client = self.client()?;
        let sent = client.send(SendRequest {
            to,
            group,
            message,
            tags,
            meta: Some(meta),
            ..Default::default()
        })?;
        self.refreshed = None;
        Ok(json!({"message": sent, "notifications": client.notifications}))
    }

    fn answer(&mut self, body: &Value) -> Result<Value> {
        let id = text(body, "id")?;
        let answer = text(body, "answer")?;
        let parent = self.parent(&id)?;
        let options = parent
            .meta
            .as_ref()
            .and_then(|m| m.pointer("/decision/options"))
            .and_then(Value::as_array)
            .context("that message is not a decision card")?;
        if !options.iter().any(|o| o.as_str() == Some(answer.as_str())) {
            bail!("answer must be one of the card's options");
        }
        let reply = self.reply(
            &id,
            format!("Answer: {answer}"),
            Some(vec!["decision".into()]),
            json!({"decision_answer": answer}),
        )?;
        self.set_dismissed(&id, true)?;
        Ok(reply)
    }

    fn carry_on(&mut self, body: &Value) -> Result<Value> {
        let id = text(body, "id")?;
        let note = body["text"].as_str().map(str::trim).filter(|t| !t.is_empty());
        self.reply(&id, note.unwrap_or("Carry on.").to_owned(), None, json!({}))
    }

    fn job(&mut self, body: &Value) -> Result<Value> {
        let id = text(body, "id")?;
        let status = text(body, "status")?;
        if !matches!(status.as_str(), "todo" | "active" | "done") {
            bail!("status must be todo, active or done");
        }
        let parent = self.parent(&id)?;
        let mut job = json!({"status": status});
        if parent.meta.as_ref().and_then(|m| m.get("job")).is_none() {
            // Turning a message into a job: its first line becomes the title.
            let title: String = text(body, "title")
                .unwrap_or_else(|_| parent.message.lines().next().unwrap_or("Job").chars().take(120).collect());
            job["title"] = Value::String(title);
        }
        self.reply(&id, format!("Job is now {status}."), Some(vec!["job".into()]), json!({"job": job}))
    }

    fn rule(&mut self, body: &Value) -> Result<Value> {
        let rule = text(body, "text")?;
        self.ensure_member(&self.team.clone())?;
        let mut client = self.client()?;
        let sent = client.send(SendRequest {
            group: Some(self.team.clone()),
            message: rule,
            tags: vec!["team-rule".into()],
            meta: Some(Self::human_meta(json!({}))),
            ..Default::default()
        })?;
        self.refreshed = None;
        Ok(json!({"message": sent}))
    }

    fn dismiss(&mut self, body: &Value) -> Result<Value> {
        let id = text(body, "id")?;
        let _: ulid::Ulid = id.parse().context("invalid message ID")?;
        let undo = body["undo"].as_bool() == Some(true);
        self.set_dismissed(&id, !undo)?;
        Ok(json!({"dismissed": self.dismissed}))
    }

    fn set_dismissed(&mut self, id: &str, dismissed: bool) -> Result<()> {
        if dismissed {
            self.dismissed.insert(id.to_owned());
        } else {
            self.dismissed.remove(id);
        }
        while self.dismissed.len() > 5000 {
            self.dismissed.pop_first();
        }
        std::fs::write(&self.dismissed_path, serde_json::to_vec(&self.dismissed)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_tags_are_lowercase_and_valid() {
        assert_eq!(hash_tags("Ship it #Release, then #needs-founder! #"), ["release", "needs-founder"]);
        assert!(hash_tags("no tags here, just C#").is_empty());
    }

    #[test]
    fn only_local_host_headers_are_allowed() {
        let hosts = allowed_hosts("127.0.0.1:7777".parse().unwrap());
        assert!(hosts.contains(&"localhost:7777".to_owned()));
        assert!(!hosts.contains(&"evil.example:7777".to_owned()));
    }
}

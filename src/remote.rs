//! Remote access for agents that cannot see the shared board folder.
//!
//! `zboard serve` runs on a machine that can see the board. Remote agents use
//! `--remote http://host:port` with a per-agent token. The serving host still
//! writes every file, so the board format is unchanged. Optional and off by
//! default: nothing listens unless `serve` is started.
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::cli::Arguments;
use crate::http::{self, Request, Response};

/// Requests per minute per token.
const RATE_LIMIT: usize = 240;
/// Failed authentications per minute per address.
const AUTH_FAILURE_LIMIT: usize = 20;
/// Largest accepted request body.
const MAX_BODY: usize = 256 * 1024;
/// Largest message body accepted from a remote agent.
const MAX_MESSAGE: usize = 64 * 1024;

/// Identity options a remote caller may not choose: the token decides them.
const IDENTITY_OPTIONS: [&str; 7] = [
    "--root",
    "--session",
    "--path",
    "--identity-project",
    "--remote",
    "--token",
    "--request-id",
];

const REMOTE_VERBS: [&str; 17] = [
    "send", "reply", "message", "history", "inbox", "search", "thread", "agents", "groups",
    "group", "identity", "tags", "ping", "status", "doctor", "activity", "id",
];

pub struct Remote {
    url: String,
    token: String,
}

impl Remote {
    pub fn new(url: String, token: Option<String>) -> Result<Self> {
        let token = token
            .or_else(|| std::env::var("ZBOARD_TOKEN").ok())
            .filter(|t| !t.is_empty())
            .context("remote access needs --token or ZBOARD_TOKEN")?;
        Ok(Self { url, token })
    }

    pub fn from_args(args: &Arguments) -> Result<Option<Self>> {
        let url = args
            .get("--remote")
            .or_else(|| std::env::var("ZBOARD_REMOTE").ok())
            .filter(|url| !url.is_empty());
        url.map(|url| Self::new(url, args.get("--token"))).transpose()
    }

    /// Forward a CLI command. `--meta-file` is read here, on the caller's
    /// machine, and sent as `--meta-json`.
    pub fn cli(&self, args: &Arguments) -> Result<Value> {
        let mut argv = args.words.clone();
        for (key, values) in &args.values {
            if IDENTITY_OPTIONS.contains(&key.as_str()) {
                continue;
            }
            for value in values {
                if key == "--meta-file" {
                    let text = std::fs::read_to_string(value).context("read --meta-file")?;
                    argv.extend(["--meta-json".to_owned(), text]);
                } else {
                    argv.extend([key.clone(), value.clone()]);
                }
            }
        }
        argv.extend(args.switches.iter().cloned());
        self.call("/v1/cli", json!({ "argv": argv }))
    }

    pub fn mcp(&self, tool: &str, arguments: Value) -> Result<Value> {
        self.call("/v1/mcp", json!({ "tool": tool, "arguments": arguments }))
    }

    fn call(&self, path: &str, body: Value) -> Result<Value> {
        let (status, value) = http::post_json(&self.url, path, Some(&self.token), &body)?;
        if status == 200 && value["ok"] == true {
            return Ok(value["data"].clone());
        }
        bail!(
            "remote {status}: {}",
            value["error"].as_str().unwrap_or("request failed")
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TokenRecord {
    pub label: String,
    pub project: String,
    pub session: String,
    pub sha256: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<String>,
}

fn token_file(board: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(board)
        .with_context(|| format!("board not found: {}", board.display()))?;
    let digest = hex(&Sha256::digest(canonical.to_string_lossy().as_bytes()));
    Ok(crate::client::local_state_root()?
        .join("serve")
        .join(&digest[..32])
        .join("tokens.json"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn load_tokens(board: &Path) -> Result<Vec<TokenRecord>> {
    match std::fs::read(token_file(board)?) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("invalid token store")?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

fn save_tokens(board: &Path, tokens: &[TokenRecord]) -> Result<()> {
    use std::io::Write;
    let path = token_file(board)?;
    let directory = path.parent().context("token directory")?;
    crate::notify::private_dir(directory)?;
    let temporary = directory.join(format!(".tokens.{}.pending", ulid::Ulid::new()));
    let mut file = crate::notify::private_file(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(tokens)?)?;
    file.sync_all()?;
    #[cfg(windows)]
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    #[cfg(unix)]
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    }
    #[cfg(not(unix))]
    {
        // ULID randomness comes from the operating system's secure source.
        bytes[..16].copy_from_slice(&ulid::Ulid::new().to_bytes());
        bytes[16..].copy_from_slice(&ulid::Ulid::new().to_bytes());
    }
    Ok(format!("zb_{}", hex(&bytes)))
}

/// `zboard token create|list|revoke`, run on the serving host by its owner.
pub fn token_command(args: impl Iterator<Item = String>) -> Result<()> {
    let args = Arguments::parse(args)?;
    let cwd = std::env::current_dir()?;
    let root = args
        .get("--root")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("ZBOARD_ROOT").map(PathBuf::from))
        .unwrap_or_else(|| crate::app::default_root(&cwd));
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut tokens = load_tokens(&root)?;
    let output = match args.words.first().map(String::as_str) {
        Some("create") => {
            let project = args.required("--project")?;
            let session = args.required("--session")?;
            crate::storage::validate_slug(&project, "project")?;
            crate::storage::normalize_session(&session)?;
            let label = args.get("--label").unwrap_or_else(|| format!("{project}-{session}"));
            if tokens.iter().any(|t| t.label == label && t.revoked_at.is_none()) {
                bail!("an active token named '{label}' exists; revoke it first or pick --label");
            }
            let token = random_token()?;
            tokens.push(TokenRecord {
                label: label.clone(),
                project,
                session,
                sha256: hex(&Sha256::digest(token.as_bytes())),
                created_at: now,
                revoked_at: None,
            });
            save_tokens(&root, &tokens)?;
            json!({"label": label, "token": token,
                "note": "Shown once. Give it only to that agent's machine, e.g. ZBOARD_TOKEN in its MCP config."})
        }
        Some("list") => {
            json!({"tokens": tokens.iter().map(|t| json!({"label": t.label, "project": t.project,
                "session": t.session, "created_at": t.created_at, "revoked_at": t.revoked_at})).collect::<Vec<_>>()})
        }
        Some("revoke") => {
            let label = args.words.get(1).context("token revoke LABEL")?;
            let mut found = false;
            for token in tokens.iter_mut().filter(|t| &t.label == label && t.revoked_at.is_none()) {
                token.revoked_at = Some(now.clone());
                found = true;
            }
            if !found {
                bail!("no active token named '{label}'");
            }
            save_tokens(&root, &tokens)?;
            json!({"revoked": label})
        }
        _ => bail!("usage: zboard token create --project P --session NAME [--label L] | token list | token revoke LABEL"),
    };
    println!("{}", json!({"ok": true, "data": output}));
    Ok(())
}

/// Addresses that are safe to listen on without `--allow-public`: loopback,
/// private LAN ranges, Tailscale/CGNAT and IPv6 unique-local addresses.
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

struct ServeState {
    root: PathBuf,
    hits: HashMap<String, (Instant, usize)>,
    servers: HashMap<String, crate::mcp::ZboardServer>,
}

fn limited(hits: &mut HashMap<String, (Instant, usize)>, key: &str, limit: usize) -> bool {
    let entry = hits.entry(key.to_owned()).or_insert((Instant::now(), 0));
    if entry.0.elapsed() >= Duration::from_secs(60) {
        *entry = (Instant::now(), 0);
    }
    entry.1 += 1;
    entry.1 > limit
}

/// `zboard serve [--root PATH] [--listen ADDR:PORT] [--allow-public]`
pub fn serve_command(mut args: impl Iterator<Item = String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let mut root = std::env::var_os("ZBOARD_ROOT").map(PathBuf::from);
    let mut listen: SocketAddr = "127.0.0.1:7780".parse()?;
    let mut allow_public = false;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--root" => root = Some(PathBuf::from(crate::app::required_value(&mut args, "--root")?)),
            "--listen" => {
                listen = crate::app::required_value(&mut args, "--listen")?
                    .parse()
                    .context("--listen must be ADDRESS:PORT")?
            }
            "--allow-public" => allow_public = true,
            other => bail!("unknown serve option '{other}'"),
        }
    }
    let root = root.unwrap_or_else(|| crate::app::default_root(&cwd));
    if !is_private(listen.ip()) && !allow_public {
        bail!(
            "{} is not a loopback, LAN or VPN address. Listen on your VPN address (e.g. Tailscale 100.x.y.z) or pass --allow-public if you really mean it",
            listen.ip()
        );
    }
    crate::storage::ensure_layout(&root)?;
    let listener = TcpListener::bind(listen).with_context(|| format!("listen on {listen}"))?;
    println!(
        "{}",
        json!({"type": "ready", "listen": listener.local_addr()?.to_string(), "root": root})
    );
    let state = Arc::new(Mutex::new(ServeState {
        root,
        hits: HashMap::new(),
        servers: HashMap::new(),
    }));
    http::serve(listener, MAX_BODY, Arc::new(move |request| handle(&state, request)))
}

fn handle(state: &Arc<Mutex<ServeState>>, request: Request) -> Response {
    if request.method != "POST" {
        return Response::error(405, "use POST");
    }
    let peer = request.peer.ip().to_string();
    let root = {
        let guard = state.lock().unwrap();
        let blocked = guard
            .hits
            .get(&format!("fail:{peer}"))
            .is_some_and(|(start, count)| start.elapsed() < Duration::from_secs(60) && *count >= AUTH_FAILURE_LIMIT);
        if blocked {
            return Response::error(429, "too many failed attempts");
        }
        guard.root.clone()
    };
    let Some(token) = request
        .header("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return auth_failure(state, &peer);
    };
    let digest = hex(&Sha256::digest(token.trim().as_bytes()));
    let record = match load_tokens(&root) {
        Ok(tokens) => tokens
            .into_iter()
            .find(|t| t.revoked_at.is_none() && constant_time_eq(t.sha256.as_bytes(), digest.as_bytes())),
        Err(error) => return Response::error(500, error),
    };
    let Some(record) = record else {
        return auth_failure(state, &peer);
    };
    if limited(&mut state.lock().unwrap().hits, &record.label, RATE_LIMIT) {
        return Response::error(429, "rate limit: 240 requests per minute");
    }
    let body = match request.json() {
        Ok(body) => body,
        Err(error) => return Response::error(400, error),
    };
    let result = match request.path.as_str() {
        "/v1/cli" => remote_cli(&root, &record, &body),
        "/v1/mcp" => remote_mcp(state, &root, &record, &body),
        _ => return Response::error(404, "unknown endpoint"),
    };
    match result {
        Ok(data) => Response::json(200, &json!({"ok": true, "data": data})),
        Err(error) => Response::json(200, &json!({"ok": false, "error": format!("{error:#}")})),
    }
}

fn auth_failure(state: &Arc<Mutex<ServeState>>, peer: &str) -> Response {
    let mut guard = state.lock().unwrap();
    if limited(&mut guard.hits, &format!("fail:{peer}"), AUTH_FAILURE_LIMIT) {
        return Response::error(429, "too many failed attempts");
    }
    Response::error(401, "missing or invalid token")
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn check_message_size(value: &Value) -> Result<()> {
    if value
        .as_str()
        .is_some_and(|text| text.len() > MAX_MESSAGE)
    {
        bail!("message exceeds {MAX_MESSAGE} bytes; split it");
    }
    Ok(())
}

fn remote_cli(root: &Path, record: &TokenRecord, body: &Value) -> Result<Value> {
    let argv: Vec<String> = serde_json::from_value(body["argv"].clone()).context("argv must be a list of strings")?;
    let args = Arguments::parse(argv.into_iter())?;
    let verb = args.words.first().context("a command is required")?.clone();
    if !REMOTE_VERBS.contains(&verb.as_str()) {
        bail!("'{verb}' is not available remotely");
    }
    if let Some(key) = args
        .values
        .keys()
        .find(|key| IDENTITY_OPTIONS.contains(&key.as_str()) || (key.as_str() == "--project" && verb != "agents"))
    {
        bail!("{key} is set by your access token, not by the request");
    }
    // Both name a file on this machine; a remote agent must not read our disk.
    if let Some(key) = ["--attach", "--meta-file"].into_iter().find(|key| args.values.contains_key(*key)) {
        bail!("{key} is not available remotely");
    }
    if let Some(message) = args.get("--message") {
        check_message_size(&Value::String(message))?;
    }
    if verb == "id" {
        return Ok(json!({"id": ulid::Ulid::new().to_string()}));
    }
    let mut client = crate::client::Client::open(root, &record.project, &record.session, root)?;
    crate::cli::run_verb(&args, &verb, &mut client)
}

fn remote_mcp(state: &Arc<Mutex<ServeState>>, root: &Path, record: &TokenRecord, body: &Value) -> Result<Value> {
    let tool = body["tool"].as_str().context("tool is required")?.to_owned();
    let arguments = body.get("arguments").cloned().unwrap_or(Value::Null);
    check_message_size(&arguments["message"])?;
    if tool == "identity_use" {
        bail!("remote agents' identity is fixed by their access token");
    }
    // Paths would name files on the serving machine, not the caller's.
    if arguments["attachments"].as_array().is_some_and(|a| !a.is_empty()) {
        bail!("attachments are not available remotely yet");
    }
    let server = {
        let mut guard = state.lock().unwrap();
        match guard.servers.get(&record.label) {
            Some(server) => server.clone(),
            None => {
                let server = crate::mcp::ZboardServer::for_identity(root, &record.project, &record.session)?;
                guard.servers.insert(record.label.clone(), server.clone());
                server
            }
        }
    };
    crate::mcp::dispatch(&server, &tool, arguments).map_err(|error| anyhow::anyhow!(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_addresses_default_to_private_networks() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.1.5", "172.16.0.1", "100.101.102.103", "::1", "fd7a:115c:a1e0::1"] {
            assert!(is_private(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["0.0.0.0", "8.8.8.8", "100.128.0.1", "2001:db8::1", "::"] {
            assert!(!is_private(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn rate_limit_window_and_constant_time_compare() {
        let mut hits = HashMap::new();
        for _ in 0..3 {
            assert!(!limited(&mut hits, "a", 3));
        }
        assert!(limited(&mut hits, "a", 3));
        assert!(!limited(&mut hits, "b", 3));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}

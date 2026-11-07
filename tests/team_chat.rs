//! End-to-end: the founder posts in the web chat, an idle agent is woken by
//! its adapter, the agent replies through the CLI, and the chat shows it.
#![cfg(unix)]
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_zboard");

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Board {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    base: PathBuf,
}

impl Board {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_owned();
        let root = base.join("board");
        let state = base.join("state");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        Self { _dir: dir, root, state, base }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.base)
            .env("XDG_STATE_HOME", &self.state)
            .env("ZBOARD_ROOT", &self.root)
            .env("ZBOARD_HOP_LIMIT", "3");
        command
    }

    fn cli(&self, session: &str, args: &[&str]) -> Value {
        let output = self
            .command()
            .args(args)
            .args(["--session", session, "--identity-project", "demo"])
            .output()
            .unwrap();
        let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "zboard {args:?} failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert_eq!(value["ok"], true, "zboard {args:?}: {value}");
        value["data"].clone()
    }

    /// An agent whose "model" is a script that replies to every wake-up.
    fn scripted_agent(&self, name: &str, reply: &str) {
        self.cli(name, &["identity"]);
        self.cli(name, &["group", "create", "team"]);
        self.cli(name, &["group", "join", "team"]);
        let script = self.base.join(format!("{name}.sh"));
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nfor id in \"$@\"; do\n  \"{BIN}\" reply \"$id\" --session {name} --identity-project demo --message '{reply}' >/dev/null\ndone\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let template = format!("{} {{ids}}", script.display());
        self.cli(
            name,
            &["notify", "register", "--adapter", "command", "--command", &template],
        );
    }

    fn web(&self) -> (Running, String) {
        let mut child = self
            .command()
            .args(["web", "--project", "demo", "--port", "0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let ready: Value = serde_json::from_str(&line).expect("web ready line");
        let url = ready["url"].as_str().unwrap().trim_end_matches('/').to_owned();
        (Running(child), url)
    }

    fn listener(&self) -> Running {
        Running(
            self.command()
                .args(["listen", "--poll-interval", "1"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
}

fn http(url: &str, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
    let address = url.strip_prefix("http://").unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    let payload = body.map(|b| b.to_string()).unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nX-Zboard: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn messages(url: &str) -> Vec<Value> {
    let (status, state) = http(url, "GET", "/api/state", None);
    assert_eq!(status, 200, "{state}");
    state["data"]["messages"].as_array().unwrap().clone()
}

#[test]
fn founder_post_wakes_agent_and_reply_shows_in_chat() {
    let board = Board::new();
    let (_web, url) = board.web();
    board.scripted_agent("coo", "On it.");
    board.scripted_agent("sales", "Not me.");

    let (status, sent) = http(&url, "POST", "/api/send", Some(&json!({"text": "@coo what's the status?"})));
    assert_eq!(status, 200, "{sent}");
    let founder_message = sent["data"]["message"].clone();
    assert_eq!(founder_message["meta"]["human"], true);
    assert_eq!(founder_message["group"], "team");
    let deliveries = sent["data"]["notifications"].as_array().unwrap();
    assert_eq!(deliveries.len(), 1, "only the mentioned agent wakes: {deliveries:?}");
    assert_eq!(deliveries[0]["agent"], "demo-coo");
    assert_eq!(deliveries[0]["status"], "started");

    let id = founder_message["id"].as_str().unwrap().to_owned();
    wait_for("the coo's reply in the chat", || {
        messages(&url)
            .iter()
            .any(|m| m["from"] == "demo-coo" && m["reply_to"] == id.as_str() && m["message"] == "On it.")
    });
    let all = messages(&url);
    assert!(!all.iter().any(|m| m["from"] == "demo-sales"), "sales was not mentioned");

    // People panel: role names, last message, and the founder marked human.
    let (_, state) = http(&url, "GET", "/api/state", None);
    let agents = state["data"]["agents"].as_array().unwrap();
    let coo = agents.iter().find(|a| a["id"] == "demo-coo").unwrap();
    assert_eq!(coo["name"], "coo");
    assert_eq!(coo["last"]["text"], "On it.");
    let founder = agents.iter().find(|a| a["id"] == "demo-founder").unwrap();
    assert_eq!(founder["human"], true);
    wait_for("coo to go idle after its turn", || {
        let (_, state) = http(&url, "GET", "/api/state", None);
        state["data"]["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == "demo-coo" && a["activity"]["state"] == "idle")
    });

    // Thread view and needs-founder: an agent asks, the founder answers.
    let asked = board.cli(
        "sales",
        &["send", "--group", "team", "--message", "Price at 9 or 12?", "--tag", "needs-founder",
          "--meta-json", r#"{"decision":{"question":"Price at 9 or 12?","options":["9","12"]}}"#],
    );
    let question = asked["id"].as_str().unwrap().to_owned();
    wait_for("the question to reach the chat", || messages(&url).iter().any(|m| m["id"] == question.as_str()));
    let (status, bad) = http(&url, "POST", "/api/answer", Some(&json!({"id": question, "answer": "15"})));
    assert_eq!(status, 400, "{bad}");
    let (status, answered) = http(&url, "POST", "/api/answer", Some(&json!({"id": question, "answer": "12"})));
    assert_eq!(status, 200, "{answered}");
    assert_eq!(answered["data"]["message"]["meta"]["decision_answer"], "12");
    let (_, thread) = http(&url, "GET", &format!("/api/thread?id={question}"), None);
    assert!(thread["data"]["messages"].as_array().unwrap().len() >= 2);

    // Cross-site requests without the custom header are refused.
    let address = url.strip_prefix("http://").unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    write!(stream, "POST /api/send HTTP/1.1\r\nHost: {address}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    let mut stream = TcpStream::connect(address).unwrap();
    write!(stream, "GET /api/state HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
}

#[test]
fn agents_waking_each_other_stop_at_the_hop_limit() {
    let board = Board::new();
    let (_web, url) = board.web();
    // Each agent answers by mentioning the other: a loop without the guard.
    board.scripted_agent("ping", "@pong your turn");
    board.scripted_agent("pong", "@ping your turn");
    // Retries wake-ups that found the other agent still busy.
    let _listener = board.listener();
    let agent_id = board.cli("ping", &["identity"])["agent"]["id"].clone();
    assert_eq!(agent_id, "demo-ping");
    board.cli("pong", &["send", "--group", "team", "--message", "@ping start"]);

    let count = || messages(&url).len();
    let mut last = 0;
    let mut stable_since = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(45);
    while stable_since.elapsed() < Duration::from_secs(6) {
        assert!(Instant::now() < deadline, "agents never stopped: {} messages", count());
        let now = count();
        if now != last {
            last = now;
            stable_since = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let all = messages(&url);
    let max_hops = all
        .iter()
        .filter_map(|m| m["meta"]["zboard_hops"].as_u64())
        .max()
        .unwrap_or(0);
    assert_eq!(max_hops, 3, "loop should stop at the hop limit: {all:#?}");
    assert!(all.len() <= 6, "too many messages: {}", all.len());

    // The founder can let them continue; that resets the count.
    let last = all.iter().max_by_key(|m| m["id"].as_str().unwrap().to_owned()).unwrap();
    let (status, carried) = http(&url, "POST", "/api/continue", Some(&json!({"id": last["id"]})));
    assert_eq!(status, 200, "{carried}");
    assert_eq!(carried["data"]["message"]["meta"]["human"], true);
}

#[test]
fn listener_wakes_agents_for_messages_from_other_hosts() {
    let board = Board::new();
    board.scripted_agent("coo", "Seen.");
    board.cli("founder", &["identity"]);
    let _listener = board.listener();
    // A sender on another host has its own local state, so it cannot wake the
    // coo directly; only the coo's host listener can.
    let other_host = board.base.join("other-host");
    std::fs::create_dir_all(&other_host).unwrap();
    let output = board
        .command()
        .env("XDG_STATE_HOME", &other_host)
        .args(["send", "--to", "demo-coo", "--message", "From afar", "--session", "founder", "--identity-project", "demo"])
        .output()
        .unwrap();
    let sent: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(sent["data"]["notifications"][0]["status"], "not_registered_locally");
    let id = sent["data"]["id"].as_str().unwrap().to_owned();
    wait_for("the listener to wake coo", || {
        board.cli("founder", &["inbox"])["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["reply_to"] == id.as_str())
    });
}

#[test]
fn remote_agent_joins_with_a_token_and_is_refused_without_one() {
    let board = Board::new();
    board.cli("coo", &["identity"]);
    let created = {
        let output = board
            .command()
            .args(["token", "create", "--project", "demo", "--session", "remote-dev"])
            .output()
            .unwrap();
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        value["data"]["token"].as_str().unwrap().to_owned()
    };
    let mut serve = board
        .command()
        .args(["serve", "--listen", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(serve.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let _serve = Running(serve);
    let ready: Value = serde_json::from_str(&line).unwrap();
    let remote = format!("http://{}", ready["listen"].as_str().unwrap());

    // The remote machine has no access to the board folder at all.
    let elsewhere = board.base.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let remote_cli = |token: &str, args: &[&str]| {
        let output = Command::new(BIN)
            .env_clear()
            .env("HOME", &elsewhere)
            .env("XDG_STATE_HOME", &elsewhere)
            .env("ZBOARD_REMOTE", &remote)
            .env("ZBOARD_TOKEN", token)
            .current_dir(&elsewhere)
            .args(args)
            .output()
            .unwrap();
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let me = remote_cli(&created, &["identity"]);
    assert_eq!(me["data"]["agent"]["id"], "demo-remotedev", "{me}");
    let sent = remote_cli(&created, &["send", "--to", "demo-coo", "--message", "Hello from the other laptop"]);
    assert_eq!(sent["ok"], true, "{sent}");
    let id = sent["data"]["id"].as_str().unwrap().to_owned();
    board.cli("coo", &["reply", &id, "--message", "Got it"]);
    let inbox = remote_cli(&created, &["inbox"]);
    assert!(
        inbox["data"]["messages"].as_array().unwrap().iter().any(|m| m["message"] == "Got it"),
        "{inbox}"
    );
    let spoof = remote_cli(&created, &["send", "--to", "demo-coo", "--message", "x", "--path", "/etc"]);
    assert_eq!(spoof["ok"], true, "--path is stripped client-side: {spoof}");
    let refused = remote_cli("zb_wrong", &["identity"]);
    assert_eq!(refused["ok"], false);
    assert!(refused["error"].as_str().unwrap().contains("401"), "{refused}");

    // Revoked tokens stop working.
    let output = board.command().args(["token", "revoke", "demo-remote-dev"]).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
    let refused = remote_cli(&created, &["identity"]);
    assert_eq!(refused["ok"], false, "{refused}");
    let _ = Path::new(&remote);
}

#[test]
fn failed_wake_up_shows_its_reason_in_the_chat() {
    let board = Board::new();
    board.cli("coo", &["identity"]);
    let status = board
        .command()
        .args(["wake-exec", "--root"])
        .arg(&board.root)
        .args(["--agent", "demo-coo", "--lock"])
        .arg(board.base.join("coo.lock"))
        .arg("--log")
        .arg(board.base.join("coo.log"))
        .args(["--ids", "X", "--", "/bin/sh", "-c", "echo 'You have hit your session limit'; exit 1"])
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let (_web, url) = board.web();
    let (_, state) = http(&url, "GET", "/api/state", None);
    let coo = state["data"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "demo-coo")
        .unwrap()
        .clone();
    assert_eq!(coo["activity"]["state"], "offline");
    assert_eq!(coo["activity"]["detail"], "Last wake-up failed: You have hit your session limit");
}

fn http_raw(url: &str, path: &str, content_type: &str, body: &[u8]) -> (u16, Value) {
    let address = url.strip_prefix("http://").unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nX-Zboard: 1\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    (head.split(' ').nth(1).unwrap().parse().unwrap(), serde_json::from_str(body).unwrap_or(Value::Null))
}

fn get_bytes(url: &str, path: &str) -> (String, Vec<u8>) {
    let address = url.strip_prefix("http://").unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n").unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    (String::from_utf8_lossy(&response[..split]).into_owned(), response[split + 4..].to_vec())
}

#[test]
fn screenshots_from_agents_and_the_founder_show_in_the_chat() {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake-image-bytes";
    let board = Board::new();
    let (_web, url) = board.web();
    board.scripted_agent("developer", "ok");

    // An agent attaches a screenshot from its machine.
    let shot = board.base.join("settings.png");
    std::fs::write(&shot, PNG).unwrap();
    let sent = board.cli(
        "developer",
        &["send", "--group", "team", "--message", "New settings page", "--attach", shot.to_str().unwrap()],
    );
    let attached = sent["message"]["meta"]["attachments"][0].clone();
    assert_eq!(attached["type"], "image/png");
    assert_eq!(attached["name"], "settings.png");
    let file = attached["file"].as_str().unwrap().to_owned();
    let (head, bytes) = get_bytes(&url, &format!("/files/{file}"));
    assert!(head.starts_with("HTTP/1.1 200") && head.contains("Content-Type: image/png"), "{head}");
    assert!(head.contains("nosniff"), "{head}");
    assert_eq!(bytes, PNG);
    assert!(get_bytes(&url, "/files/../board/v1").0.starts_with("HTTP/1.1 404"));

    // Not an image: refused.
    let notes = board.base.join("notes.png");
    std::fs::write(&notes, b"secret text").unwrap();
    let output = board
        .command()
        .args(["send", "--group", "team", "--message", "x", "--attach", notes.to_str().unwrap()])
        .args(["--session", "developer", "--identity-project", "demo"])
        .output()
        .unwrap();
    assert!(!output.status.success());

    // The founder pastes a screenshot and sends it with no text.
    let (status, up) = http_raw(&url, "/api/upload?name=Screen%20Shot.png", "image/png", PNG);
    assert_eq!(status, 200, "{up}");
    assert_eq!(up["data"]["name"], "Screen Shot.png");
    let (status, posted) = http(&url, "POST", "/api/send", Some(&json!({"text": "", "attachments": [up["data"]]})));
    assert_eq!(status, 200, "{posted}");
    assert_eq!(posted["data"]["message"]["meta"]["attachments"][0]["file"], up["data"]["file"]);
    assert_eq!(posted["data"]["message"]["meta"]["human"], true);
    // A claim about a file that isn't on the board is refused.
    let (status, _) = http(&url, "POST", "/api/send", Some(&json!({"text": "x",
        "attachments": [{"file": "01m6tmkvr4tq35fkgh2nf2295z.png"}]})));
    assert_eq!(status, 400);
    let (status, _) = http_raw(&url, "/api/upload?name=x.html", "text/html", b"<script>alert(1)</script>");
    assert_eq!(status, 400);
}

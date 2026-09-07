use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdout, Command, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

fn start(root: &TempDir, project: &str, session: &str) -> (Child, BufReader<ChildStdout>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zboard"))
        .args([
            "run",
            "--root",
            root.path().to_str().unwrap(),
            "--project",
            project,
            "--session",
            session,
            "--poll-interval",
            "1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    (child, stdout)
}

fn next_event(output: &mut impl BufRead) -> Value {
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    assert!(!line.is_empty(), "zboard exited before the expected event");
    serde_json::from_str(&line).unwrap()
}

fn stop(mut child: Child) {
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
}

#[test]
fn jsonl_restart_emits_message_received_while_offline() {
    let root = TempDir::new().unwrap();
    let (recipient, mut recipient_output) = start(&root, "infra", "two");
    let ready = next_event(&mut recipient_output);
    assert_eq!(ready["type"], "ready");
    let recipient_id = ready["agent"].as_str().unwrap().to_owned();
    stop(recipient);

    let (mut sender, mut sender_output) = start(&root, "worka", "one");
    assert_eq!(next_event(&mut sender_output)["type"], "ready");
    writeln!(
        sender.stdin.as_mut().unwrap(),
        "{}",
        json!({"op":"send","to":[recipient_id],"message":"while offline"})
    )
    .unwrap();
    sender.stdin.as_mut().unwrap().flush().unwrap();
    let sent = next_event(&mut sender_output);
    assert_eq!(sent["type"], "sent");
    stop(sender);

    let (resumed, mut resumed_output) = start(&root, "infra", "two");
    assert_eq!(next_event(&mut resumed_output)["type"], "ready");
    let delivered = next_event(&mut resumed_output);
    assert_eq!(delivered["type"], "message");
    assert_eq!(delivered["message"]["message"], "while offline");
    stop(resumed);
}

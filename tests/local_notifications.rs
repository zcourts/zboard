#![cfg(unix)]
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

struct WakeProcess(std::process::Child);

impl Drop for WakeProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_checkpoint(child: &mut WakeProcess, checkpoint: &std::path::Path, offset: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        assert!(child.0.try_wait().unwrap().is_none(), "wake exited early");
        if std::fs::read(checkpoint)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|value| value["offset"].as_u64() == Some(offset))
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "wake checkpoint timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

#[test]
fn receiver_bridge_queues_remote_messages_and_shares_direct_delivery_receipts() {
    use std::io::Write;
    use std::process::Stdio;

    let helper = include_str!("../scripts/codex-board");
    let wake_setup = helper.split("wake_pid=$!").next().unwrap();
    assert!(wake_setup.contains("--root \"$board\""));
    assert!(wake_setup.contains("\"$binary\" wake "));

    let dir = tempfile::tempdir().unwrap();
    let board = dir.path().join("board");
    let local = dir.path().join("receiver-state");
    let remote = dir.path().join("sender-state");
    let codex = dir.path().join("fake-codex");
    std::fs::write(
        &codex,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
    let thread = "00000000-0000-4000-8000-000000000002";
    let receiver = "receiver-00000000000040008000000000000002";
    let run = |state: &std::path::Path, project: &str, session: &str, args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_zboard"))
            .env("XDG_STATE_HOME", state)
            .env("ZBOARD_ROOT", &board)
            .env("ZBOARD_PROJECT", project)
            .env("ZBOARD_SESSION_ID", session)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["data"].clone()
    };
    run(
        &local,
        "receiver",
        thread,
        &["notify", "register", "--codex", codex.to_str().unwrap()],
    );
    let events = dir.path().join("events.jsonl");
    let checkpoint = dir.path().join("wake.json");
    std::fs::write(&events, "").unwrap();
    let mut child = WakeProcess(
        Command::new(env!("CARGO_BIN_EXE_zboard"))
            .env("XDG_STATE_HOME", &local)
            .args(["wake", "--root"])
            .arg(&board)
            .arg("--events")
            .arg(&events)
            .arg("--state")
            .arg(&checkpoint)
            .args(["--agent", receiver, "--thread", thread, "--codex"])
            .arg(&codex)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for_checkpoint(&mut child, &checkpoint, 0);

    let mut offset = 0;
    for (state, expected, body) in [
        (&remote, "not_registered_locally", "remote sender"),
        (&local, "queued", "same-host sender"),
    ] {
        let sent = run(
            state,
            "sender",
            "sender",
            &["send", "--to", receiver, "--message", body],
        );
        assert_eq!(sent["persisted"], true);
        assert_eq!(sent["notifications"][0]["status"], expected);
        let id = sent["id"].as_str().unwrap();
        if expected == "not_registered_locally" {
            assert!(!codex.with_extension("calls").exists());
        }
        // Feed the receiver bridge the same event produced by its JSONL listener.
        let mut event =
            serde_json::to_vec(&serde_json::json!({"type":"message", "message":sent["message"]}))
                .unwrap();
        event.push(b'\n');
        std::fs::OpenOptions::new()
            .append(true)
            .open(&events)
            .unwrap()
            .write_all(&event)
            .unwrap();
        offset += event.len() as u64;
        wait_for_checkpoint(&mut child, &checkpoint, offset);
        let calls = std::fs::read_to_string(codex.with_extension("calls")).unwrap();
        assert_eq!(calls.lines().filter(|line| line.contains(id)).count(), 1);
        assert_eq!(
            run(&local, "sender", "sender", &["notify", "retry", id])["notifications"][0]["status"],
            "already_queued"
        );
    }
    assert_eq!(
        std::fs::read_to_string(codex.with_extension("calls"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[test]
fn cli_persists_then_notifies_without_a_listener_and_can_retry_failed_notification() {
    let dir = tempfile::tempdir().unwrap();
    let board = dir.path().join("board");
    let local = dir.path().join("state");
    let codex = dir.path().join("fake-codex");
    std::fs::write(
        &codex,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
    let thread = "00000000-0000-4000-8000-000000000001";
    let run = |project: &str, session: &str, args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_zboard"))
            .env("XDG_STATE_HOME", &local)
            .env("ZBOARD_ROOT", &board)
            .env("ZBOARD_PROJECT", project)
            .env("ZBOARD_SESSION_ID", session)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["data"].clone()
    };
    run(
        "receiver",
        thread,
        &["notify", "register", "--codex", codex.to_str().unwrap()],
    );
    let receiver = "receiver-00000000000040008000000000000001";
    let sent = run(
        "sender",
        "sender",
        &["send", "--to", receiver, "--message", "hello"],
    );
    assert_eq!(sent["persisted"], true);
    assert_eq!(sent["notifications"][0]["status"], "queued");
    let id = sent["id"].as_str().unwrap();
    assert!(
        std::fs::read_to_string(codex.with_extension("calls"))
            .unwrap()
            .contains(id)
    );
    let retry = run("sender", "sender", &["notify", "retry", id]);
    assert_eq!(retry["notifications"][0]["status"], "already_queued");
    // A receiver on a different VM has no local registration but still reads the message.
    let remote = Command::new(env!("CARGO_BIN_EXE_zboard"))
        .env("XDG_STATE_HOME", dir.path().join("other-vm"))
        .env("ZBOARD_ROOT", &board)
        .env("ZBOARD_PROJECT", "receiver")
        .env("ZBOARD_SESSION_ID", thread)
        .args(["message", "get", id])
        .output()
        .unwrap();
    assert!(remote.status.success());
    assert!(String::from_utf8_lossy(&remote.stdout).contains("hello"));
    std::fs::write(&codex, "#!/bin/sh\necho unavailable >&2\nexit 1\n").unwrap();
    let sent = run(
        "sender",
        "sender",
        &[
            "send",
            "--to",
            receiver,
            "--message",
            "durable despite daemon failure",
        ],
    );
    assert_eq!(sent["persisted"], true);
    assert_eq!(sent["notifications"][0]["status"], "failed");
    let id = sent["id"].as_str().unwrap();
    assert_eq!(
        run("receiver", thread, &["message", "get", id])["messages"][0]["message"],
        "durable despite daemon failure"
    );
    std::fs::write(&codex, "#!/bin/sh\nexit 0\n").unwrap();
    assert_eq!(
        run("sender", "sender", &["notify", "retry", id])["notifications"][0]["status"],
        "queued"
    );
    run("receiver", thread, &["notify", "unregister"]);
    let sent = run(
        "sender",
        "sender",
        &["send", "--to", receiver, "--message", "no automatic wake"],
    );
    assert_eq!(sent["notifications"][0]["status"], "not_registered_locally");
}

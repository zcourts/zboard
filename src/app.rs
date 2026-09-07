use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::model::{Command, GroupSummary, Output};
use crate::storage::{
    BoardState, MessageDraft, acknowledge_messages, agent_statuses, create_group,
    direct_reply_recipients, ensure_layout, group_members, initialize_routing, join_group,
    message_is_relevant, publish_message, register_agent, relevant_history, scan, touch_presence,
};

pub struct RunOptions {
    pub root: PathBuf,
    pub project: String,
    pub session: String,
    pub project_path: PathBuf,
    pub poll_interval: Duration,
}

enum LoopEvent {
    Input(String),
    InputClosed,
    Filesystem,
    WatchWarning(String),
}

pub fn run(options: RunOptions) -> Result<()> {
    let version_root = ensure_layout(&options.root)?;
    let agent = register_agent(
        &version_root,
        &options.project,
        &options.session,
        std::env::consts::OS,
        &options.project_path,
    )?;
    touch_presence(&version_root, &agent.id)?;
    let mut state = BoardState::default();
    let resumed = initialize_routing(&version_root, &mut state, &agent)?;
    let initial = scan(&version_root, &mut state);
    let initial_messages: Vec<_> = initial
        .new_messages
        .iter()
        .filter_map(|id| state.messages.get(id).cloned())
        .collect();
    if !resumed {
        acknowledge_messages(&version_root, &mut state, &initial_messages)?;
    }

    let stdout = io::stdout();
    let mut output = stdout.lock();
    for warning in initial.warnings {
        emit(&mut output, &Output::Warning { message: &warning })?;
    }
    emit(
        &mut output,
        &Output::Ready {
            agent: &agent.id,
            latest: state.messages.keys().next_back().map(String::as_str),
        },
    )?;
    if resumed {
        let mut delivered = Vec::new();
        for message in &initial_messages {
            if message_is_relevant(&state, &agent, message) {
                emit(&mut output, &Output::Incoming { message })?;
                delivered.push(message.clone());
            }
        }
        acknowledge_messages(&version_root, &mut state, &delivered)?;
    }

    let (event_tx, event_rx) = mpsc::channel();
    spawn_stdin_reader(event_tx.clone());
    let filesystem_pending = Arc::new(AtomicBool::new(false));
    let _watcher = start_native_watcher(
        &options.root,
        event_tx.clone(),
        Arc::clone(&filesystem_pending),
        &mut output,
    );

    event_loop(
        &version_root,
        &agent,
        &mut state,
        event_rx,
        &filesystem_pending,
        options.poll_interval,
        &mut output,
    )
}

fn event_loop(
    version_root: &Path,
    agent: &crate::model::Agent,
    state: &mut BoardState,
    event_rx: Receiver<LoopEvent>,
    filesystem_pending: &AtomicBool,
    poll_interval: Duration,
    output: &mut impl Write,
) -> Result<()> {
    let mut reconcile_at = std::time::Instant::now() + poll_interval;
    loop {
        let timeout = reconcile_at.saturating_duration_since(std::time::Instant::now());
        let mut reconcile_due = false;
        match event_rx.recv_timeout(timeout) {
            Ok(LoopEvent::Input(line)) => handle_line(version_root, agent, state, &line, output)?,
            Ok(LoopEvent::InputClosed) => return Ok(()),
            Ok(LoopEvent::WatchWarning(message)) => {
                emit(output, &Output::Warning { message: &message })?
            }
            Ok(LoopEvent::Filesystem) => {
                filesystem_pending.store(false, Ordering::Release);
                reconcile_at =
                    reconcile_at.min(std::time::Instant::now() + Duration::from_millis(50));
            }
            Err(RecvTimeoutError::Timeout) => reconcile_due = true,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
        if reconcile_due || std::time::Instant::now() >= reconcile_at {
            reconcile(version_root, agent, state, output)?;
            reconcile_at = std::time::Instant::now() + poll_interval;
        }
    }
}

fn handle_line(
    version_root: &Path,
    agent: &crate::model::Agent,
    state: &mut BoardState,
    line: &str,
    output: &mut impl Write,
) -> Result<()> {
    let command = match serde_json::from_str::<Command>(line) {
        Ok(command) => command,
        Err(error) => {
            let message = format!("invalid command JSON: {error}");
            return emit(
                output,
                &Output::Error {
                    op: None,
                    message: &message,
                },
            );
        }
    };

    let operation = command_name(&command);
    if let Err(error) = handle_command(version_root, agent, state, command, output) {
        let message = format!("{error:#}");
        emit(
            output,
            &Output::Error {
                op: Some(operation),
                message: &message,
            },
        )?;
    }
    Ok(())
}

fn handle_command(
    version_root: &Path,
    agent: &crate::model::Agent,
    state: &mut BoardState,
    command: Command,
    output: &mut impl Write,
) -> Result<()> {
    let presence = touch_presence(version_root, &agent.id)?;
    state.presence.insert(agent.id.clone(), presence);
    match command {
        Command::Send {
            to,
            group,
            message,
            meta,
            ttl_seconds,
        } => {
            if let Some(group) = group.as_deref() {
                validate_target_group(state, agent, group)?;
            } else {
                validate_recipients(state, &to)?;
            }
            let message = publish_message(
                version_root,
                &agent.id,
                MessageDraft {
                    to,
                    group,
                    thread: None,
                    reply_to: None,
                    body: message,
                    meta,
                    ttl_seconds,
                },
            )?;
            acknowledge_messages(version_root, state, std::slice::from_ref(&message))?;
            state.messages.insert(message.id.clone(), message.clone());
            emit(output, &Output::Sent { id: &message.id })?;
        }
        Command::Reply {
            to,
            message,
            meta,
            ttl_seconds,
        } => {
            let parent = state
                .messages
                .get(&to)
                .cloned()
                .ok_or_else(|| anyhow!("unknown message '{to}'"))?;
            let group = parent.group.clone();
            let recipients = if group.is_some() {
                Vec::new()
            } else {
                direct_reply_recipients(&parent, &agent.id)
            };
            if group.is_none() && recipients.is_empty() {
                bail!("reply has no participant other than the current agent");
            }
            let message = publish_message(
                version_root,
                &agent.id,
                MessageDraft {
                    to: recipients,
                    group,
                    thread: Some(parent.thread),
                    reply_to: Some(parent.id),
                    body: message,
                    meta,
                    ttl_seconds,
                },
            )?;
            acknowledge_messages(version_root, state, std::slice::from_ref(&message))?;
            state.messages.insert(message.id.clone(), message.clone());
            emit(output, &Output::Sent { id: &message.id })?;
        }
        Command::GroupCreate { name } => {
            let group = create_group(version_root, &name, &agent.id)?;
            state.groups.insert(group.name.clone(), group);
            emit(output, &Output::GroupCreated { name: &name })?;
        }
        Command::GroupJoin { name } => {
            let membership = join_group(version_root, &name, &agent.id)?;
            state
                .memberships
                .insert((membership.group.clone(), membership.agent));
            emit(output, &Output::GroupJoined { name: &name })?;
        }
        Command::Agents => {
            emit(
                output,
                &Output::Agents {
                    agents: agent_statuses(state),
                },
            )?;
        }
        Command::Groups => {
            let groups = state
                .groups
                .values()
                .map(|group| GroupSummary {
                    name: &group.name,
                    created_by: &group.created_by,
                    members: group_members(state, &group.name),
                })
                .collect();
            emit(output, &Output::Groups { groups })?;
        }
        Command::History {
            thread,
            group,
            limit,
        } => {
            let limit = limit.unwrap_or(50).min(1_000);
            let (messages, warnings) = relevant_history(
                version_root,
                state,
                agent,
                thread.as_deref(),
                group.as_deref(),
                limit,
            );
            for warning in warnings {
                emit(output, &Output::Warning { message: &warning })?;
            }
            emit(
                output,
                &Output::History {
                    messages: messages.iter().collect(),
                },
            )?;
        }
        Command::Ping => emit(output, &Output::Pong)?,
    }
    Ok(())
}

fn validate_recipients(state: &BoardState, recipients: &[String]) -> Result<()> {
    if recipients.is_empty() {
        bail!("provide at least one recipient");
    }
    let unknown: Vec<_> = recipients
        .iter()
        .filter(|recipient| !state.agents.contains_key(recipient.as_str()))
        .collect();
    if !unknown.is_empty() {
        bail!("unknown recipient(s): {unknown:?}");
    }
    Ok(())
}

fn validate_target_group(
    state: &BoardState,
    agent: &crate::model::Agent,
    group: &str,
) -> Result<()> {
    if group == "global" || group == format!("project:{}", agent.project) {
        return Ok(());
    }
    if !state.groups.contains_key(group) {
        bail!("unknown group '{group}'");
    }
    if !state
        .memberships
        .contains(&(group.to_owned(), agent.id.clone()))
    {
        bail!("join group '{group}' before sending to it");
    }
    Ok(())
}

fn reconcile(
    version_root: &Path,
    agent: &crate::model::Agent,
    state: &mut BoardState,
    output: &mut impl Write,
) -> Result<()> {
    let result = scan(version_root, state);
    for warning in result.warnings {
        emit(output, &Output::Warning { message: &warning })?;
    }
    let mut acknowledged = Vec::new();
    for id in result.new_messages {
        let Some(message) = state.messages.get(&id) else {
            continue;
        };
        if message_is_relevant(state, agent, message) {
            emit(output, &Output::Incoming { message })?;
        }
        acknowledged.push(message.clone());
    }
    acknowledge_messages(version_root, state, &acknowledged)?;
    Ok(())
}

fn spawn_stdin_reader(event_tx: Sender<LoopEvent>) {
    thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(line) => {
                    if event_tx.send(LoopEvent::Input(line)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = event_tx.send(LoopEvent::WatchWarning(format!(
                        "stdin read failed: {error}"
                    )));
                    break;
                }
            }
        }
        let _ = event_tx.send(LoopEvent::InputClosed);
    });
}

fn start_native_watcher(
    board_root: &Path,
    event_tx: Sender<LoopEvent>,
    filesystem_pending: Arc<AtomicBool>,
    output: &mut impl Write,
) -> Option<RecommendedWatcher> {
    let callback_tx = event_tx.clone();
    let watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(event)
                if matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                ) =>
            {
                if !filesystem_pending.swap(true, Ordering::AcqRel) {
                    let _ = callback_tx.send(LoopEvent::Filesystem);
                }
            }
            Ok(_) => {}
            Err(error) => {
                let _ = callback_tx.send(LoopEvent::WatchWarning(format!(
                    "native watcher error: {error}"
                )));
            }
        });
    let mut watcher = match watcher {
        Ok(watcher) => watcher,
        Err(error) => {
            let message = format!("native watcher unavailable; polling remains active: {error}");
            let _ = emit(output, &Output::Warning { message: &message });
            return None;
        }
    };
    if let Err(error) = watcher.watch(board_root, RecursiveMode::Recursive) {
        let message = format!("native watcher unavailable; polling remains active: {error}");
        let _ = emit(output, &Output::Warning { message: &message });
        return None;
    }
    Some(watcher)
}

fn emit(output: &mut impl Write, event: &Output<'_>) -> Result<()> {
    serde_json::to_writer(&mut *output, event).context("write output event")?;
    output.write_all(b"\n").context("finish output event")?;
    output.flush().context("flush output event")?;
    Ok(())
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Send { .. } => "send",
        Command::Reply { .. } => "reply",
        Command::GroupCreate { .. } => "group.create",
        Command::GroupJoin { .. } => "group.join",
        Command::Agents => "agents",
        Command::Groups => "groups",
        Command::History { .. } => "history",
        Command::Ping => "ping",
    }
}

pub fn default_root(start: &Path) -> PathBuf {
    for ancestor in start.ancestors() {
        let candidate = ancestor.join(".ai").join("message-board");
        if candidate.exists() {
            return candidate;
        }
    }
    start.join(".ai").join("message-board")
}

pub fn parse_duration_seconds(value: &str) -> Result<Duration> {
    let seconds: u64 = value.parse().context("poll interval must be seconds")?;
    if seconds == 0 {
        bail!("poll interval must be at least one second");
    }
    Ok(Duration::from_secs(seconds))
}

pub fn required_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| anyhow!("{flag} requires a value"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AGENT_SCHEMA, Agent};
    use crate::storage::publish_message;
    use tempfile::tempdir;

    fn agent() -> Agent {
        Agent {
            schema: AGENT_SCHEMA.to_owned(),
            id: "infra-session".to_owned(),
            project: "infra".to_owned(),
            session_id: "session".to_owned(),
            platform: "linux".to_owned(),
            path: "/infra".to_owned(),
            registered_at: "2026-09-06T00:00:00.000Z".to_owned(),
        }
    }

    #[test]
    fn malformed_command_is_nonfatal() {
        let directory = tempdir().unwrap();
        let root = ensure_layout(directory.path()).unwrap();
        let mut state = BoardState::default();
        let mut output = Vec::new();
        handle_line(&root, &agent(), &mut state, "not-json", &mut output).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["type"], "error");
    }

    #[test]
    fn reply_preserves_thread_and_reconstructs_participants() {
        let directory = tempdir().unwrap();
        let root = ensure_layout(directory.path()).unwrap();
        let me = agent();
        let other = Agent {
            id: "worka-other".to_owned(),
            project: "worka".to_owned(),
            session_id: "other".to_owned(),
            ..me.clone()
        };
        let mut state = BoardState::default();
        state.agents.insert(me.id.clone(), me.clone());
        state.agents.insert(other.id.clone(), other.clone());
        let parent = publish_message(
            &root,
            &other.id,
            MessageDraft {
                to: vec![me.id.clone()],
                group: None,
                thread: None,
                reply_to: None,
                body: "question".to_owned(),
                meta: None,
                ttl_seconds: None,
            },
        )
        .unwrap();
        state.messages.insert(parent.id.clone(), parent.clone());

        let mut output = Vec::new();
        handle_command(
            &root,
            &me,
            &mut state,
            Command::Reply {
                to: parent.id.clone(),
                message: "answer".to_owned(),
                meta: None,
                ttl_seconds: None,
            },
            &mut output,
        )
        .unwrap();
        let reply = state.messages.values().next_back().unwrap();
        assert_eq!(reply.thread, parent.thread);
        assert_eq!(reply.reply_to.as_deref(), Some(parent.id.as_str()));
        assert_eq!(reply.to, vec![other.id]);
    }

    #[test]
    fn history_is_ordered_and_limited() {
        let directory = tempdir().unwrap();
        let root = ensure_layout(directory.path()).unwrap();
        let mut state = BoardState::default();
        for body in ["one", "two", "three"] {
            let message = publish_message(
                &root,
                "worka-other",
                MessageDraft {
                    to: vec!["infra-session".to_owned()],
                    group: None,
                    thread: None,
                    reply_to: None,
                    body: body.to_owned(),
                    meta: None,
                    ttl_seconds: None,
                },
            )
            .unwrap();
            state.messages.insert(message.id.clone(), message);
        }
        let mut output = Vec::new();
        handle_command(
            &root,
            &agent(),
            &mut state,
            Command::History {
                thread: None,
                group: None,
                limit: Some(2),
            },
            &mut output,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["messages"].as_array().unwrap().len(), 2);
        assert_eq!(value["messages"][1]["message"], "three");
    }

    #[test]
    fn history_excludes_unrelated_conversations() {
        let directory = tempdir().unwrap();
        let root = ensure_layout(directory.path()).unwrap();
        let mut state = BoardState::default();
        for (recipients, body) in [
            (vec!["infra-session".to_owned()], "relevant"),
            (vec!["keldra-other".to_owned()], "unrelated"),
        ] {
            let message = publish_message(
                &root,
                "worka-other",
                MessageDraft {
                    to: recipients,
                    group: None,
                    thread: None,
                    reply_to: None,
                    body: body.to_owned(),
                    meta: None,
                    ttl_seconds: None,
                },
            )
            .unwrap();
            state.messages.insert(message.id.clone(), message);
        }

        let mut output = Vec::new();
        handle_command(
            &root,
            &agent(),
            &mut state,
            Command::History {
                thread: None,
                group: None,
                limit: None,
            },
            &mut output,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["messages"].as_array().unwrap().len(), 1);
        assert_eq!(value["messages"][0]["message"], "relevant");
    }
}

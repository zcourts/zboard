//! One-shot commands never consume a listener's inbox or checkpoint.
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::client::{Client, ReplyRequest, SendRequest};
use crate::{query, routed, storage};

#[derive(Debug, Default)]
struct Arguments {
    words: Vec<String>,
    values: BTreeMap<String, Vec<String>>,
    switches: BTreeSet<String>,
}

impl Arguments {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut result = Self::default();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            if arg == "--" {
                result.words.extend(args);
                break;
            }
            if matches!(
                arg.as_str(),
                "--online" | "--joined" | "--clear-tags" | "--notify-only" | "--help" | "-h"
            ) {
                result.switches.insert(arg);
            } else if arg.starts_with("--") {
                let (key, value) = if let Some((key, value)) = arg.split_once('=') {
                    (key.to_owned(), value.to_owned())
                } else {
                    let value = args
                        .next()
                        .with_context(|| format!("missing value for {arg}"))?;
                    if value.starts_with("--") {
                        bail!("missing value for {arg}");
                    }
                    (arg, value)
                };
                result.values.entry(key).or_default().push(value);
            } else {
                result.words.push(arg);
            }
        }
        for (key, values) in &result.values {
            if values.len() > 1 && !matches!(key.as_str(), "--to" | "--tag") {
                bail!("option {key} may only be supplied once");
            }
        }
        Ok(result)
    }

    fn get(&self, key: &str) -> Option<String> {
        self.values.get(key).and_then(|v| v.first()).cloned()
    }
    fn required(&self, key: &str) -> Result<String> {
        self.get(key)
            .with_context(|| format!("required option {key}"))
    }
    fn many(&self, key: &str) -> Vec<String> {
        self.values.get(key).cloned().unwrap_or_default()
    }
    fn flag(&self, key: &str) -> bool {
        self.switches.contains(key)
    }
    fn validate(&self, allowed: &[&str], switches: &[&str]) -> Result<()> {
        for key in self.values.keys() {
            if ![
                "--root",
                "--session",
                "--path",
                "--project",
                "--identity-project",
                "--request-id",
            ]
            .contains(&key.as_str())
                && !allowed.contains(&key.as_str())
            {
                bail!("unsupported option {key}");
            }
        }
        for key in &self.switches {
            if !switches.contains(&key.as_str()) {
                bail!("unsupported option {key}");
            }
        }
        Ok(())
    }
    fn words(&self, count: usize) -> Result<()> {
        if self.words.len() != count {
            bail!("unexpected command arguments: {:?}", self.words);
        }
        Ok(())
    }
}

pub fn command(args: impl Iterator<Item = String>) -> Result<()> {
    let arguments = match Arguments::parse(args) {
        Ok(arguments) => arguments,
        Err(error) => {
            println!(
                "{}",
                json!({"ok":false,"request_id":ulid::Ulid::new().to_string(),"error":format!("{error:#}")})
            );
            return Err(error);
        }
    };
    if arguments.flag("--help")
        || arguments.flag("-h")
        || arguments.words.first().is_some_and(|s| s == "help")
    {
        println!("{}", help());
        return Ok(());
    }
    let request_id = arguments
        .get("--request-id")
        .unwrap_or_else(|| ulid::Ulid::new().to_string());
    let result = execute(&arguments);
    match result {
        Ok(data) => println!("{}", json!({"ok":true,"request_id":request_id,"data":data})),
        Err(error) => {
            println!(
                "{}",
                json!({"ok":false,"request_id":request_id,"error":format!("{error:#}")})
            );
            return Err(error);
        }
    }
    Ok(())
}

fn open(args: &Arguments, command: &str) -> Result<Client> {
    let cwd = env::current_dir().context("read current directory")?;
    let root = args
        .get("--root")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("ZBOARD_ROOT")
                .or_else(|| env::var_os("AIBOARD_ROOT"))
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| crate::app::default_root(&cwd));
    let session = args
        .get("--session")
        .or_else(|| {
            [
                "ZBOARD_SESSION_ID",
                "AIBOARD_SESSION_ID",
                "CODEX_THREAD_ID",
                "CODEX_SESSION_ID",
                "CLAUDE_SESSION_ID",
            ]
            .iter()
            .find_map(|key| env::var(key).ok())
        })
        .context("set --session or a stable agent session environment variable")?;
    let explicit_project = args
        .get("--identity-project")
        .or_else(|| {
            if command == "agents" {
                None
            } else {
                args.get("--project")
            }
        })
        .or_else(|| {
            env::var("ZBOARD_PROJECT")
                .or_else(|_| env::var("AIBOARD_PROJECT"))
                .ok()
        });
    let mut path = args
        .get("--path")
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.clone());
    let project = if let Some(project) = explicit_project {
        project
    } else if !root.join("v1").exists() {
        crate::project_slug(&cwd)
    } else {
        let mut state = storage::BoardState::default();
        let report = storage::scan_metadata(&root.join("v1"), &mut state);
        if !report.warnings.is_empty() {
            bail!("cannot resolve identity: {}", report.warnings.join("; "));
        }
        let matches: Vec<_> = state
            .agents
            .values()
            .filter(|agent| agent.session_id == session)
            .collect();
        match matches.as_slice() {
            [agent] => {
                if args.get("--path").is_none() {
                    path = PathBuf::from(&agent.path);
                }
                agent.project.clone()
            }
            [] => crate::project_slug(&cwd),
            _ => bail!("session belongs to multiple projects; set --identity-project"),
        }
    };
    if matches!(command, "send" | "reply" | "ping" | "group" | "identity") {
        Client::open(&root, &project, &session, &path)
    } else {
        Client::read_only(&root, &project, &session, &path)
    }
}

fn meta(args: &Arguments) -> Result<Option<Value>> {
    match (args.get("--meta-json"), args.get("--meta-file")) {
        (Some(_), Some(_)) => bail!("use only one of --meta-json and --meta-file"),
        (Some(value), None) => Ok(Some(
            serde_json::from_str(&value).context("parse --meta-json")?,
        )),
        (None, Some(path)) => Ok(Some(
            serde_json::from_reader(std::fs::File::open(path).context("open --meta-file")?)
                .context("parse --meta-file")?,
        )),
        (None, None) => Ok(None),
    }
}

fn limit(args: &Arguments) -> Result<Option<usize>> {
    args.get("--limit")
        .map(|v| v.parse::<usize>())
        .transpose()
        .context("invalid --limit")
}

fn watch(client: &mut Client, args: &Arguments) -> Result<Value> {
    use std::io::Write;
    args.validate(&["--after", "--poll-seconds"], &["--notify-only"])?;
    args.words(1)?;
    let interval = args
        .get("--poll-seconds")
        .map(|v| v.parse::<u64>())
        .transpose()
        .context("invalid --poll-seconds")?
        .unwrap_or(3);
    if !(1..=300).contains(&interval) {
        bail!("--poll-seconds must be between 1 and 300");
    }
    let mut seen = BTreeMap::<String, ()>::new();
    let mut initial = true;
    loop {
        client.state = query::snapshot(&client.root)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;
        let window = ulid::Ulid::from_parts(now.saturating_sub(120_000), 0).to_string();
        let mut cursor = if initial {
            args.get("--after").unwrap_or_else(|| window.clone())
        } else {
            window.clone()
        };
        loop {
            let result = query::history(
                &client.root,
                &client.state,
                &client.agent,
                &query::HistoryQuery {
                    after: Some(cursor.clone()),
                    limit: 1000,
                    ..Default::default()
                },
            )?;
            for warning in result.warnings {
                println!("{}", json!({"type":"warning","message":warning}));
            }
            for message in result.messages {
                if seen.insert(message.id.clone(), ()).is_some() {
                    continue;
                }
                if initial && args.get("--after").is_none() {
                    continue;
                }
                if args.flag("--notify-only")
                    && (message.from == client.agent.id
                        || !(message.to.contains(&client.agent.id)
                            || message.tags.iter().any(|t| t == "notify")
                            || message
                                .meta
                                .as_ref()
                                .and_then(|m| m.get("notify"))
                                .and_then(Value::as_bool)
                                == Some(true)))
                {
                    continue;
                }
                println!("{}", json!({"type":"message","message":message}));
            }
            if let Some(next) = result.next_cursor {
                cursor = next;
            } else {
                break;
            }
        }
        if initial {
            println!(
                "{}",
                json!({"type":"ready","agent":client.agent.id,"mode":"read-only-watch","late_arrival_overlap_seconds":120})
            );
        }
        std::io::stdout().flush()?;
        seen.retain(|id, _| id >= &window);
        initial = false;
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

fn execute(args: &Arguments) -> Result<Value> {
    let verb = args
        .words
        .first()
        .context("a command is required")?
        .as_str();
    if verb == "id" {
        args.validate(&[], &[])?;
        args.words(2)?;
        if args.words[1] != "new" {
            bail!("expected id new");
        }
        return Ok(json!({"id":ulid::Ulid::new().to_string()}));
    }
    let mut client = open(args, verb)?;
    match verb {
        "watch" => watch(&mut client, args),
        "send" | "reply" => {
            let mut allowed = vec![
                "--message",
                "--tag",
                "--meta-json",
                "--meta-file",
                "--ttl-seconds",
                "--idempotency-key",
            ];
            if verb == "send" {
                allowed.extend(["--to", "--group"]);
            }
            args.validate(
                &allowed,
                if verb == "reply" {
                    &["--clear-tags"]
                } else {
                    &[]
                },
            )?;
            args.words(if verb == "send" { 1 } else { 2 })?;
            let message = args.required("--message")?;
            let metadata = meta(args)?;
            let ttl_seconds = args
                .get("--ttl-seconds")
                .map(|value| value.parse::<u64>())
                .transpose()
                .context("--ttl-seconds must be an integer")?;
            let key = args
                .get("--idempotency-key")
                .unwrap_or_else(|| ulid::Ulid::new().to_string());
            if args.flag("--clear-tags") && !args.many("--tag").is_empty() {
                bail!("--clear-tags conflicts with --tag");
            }
            let result = if verb == "send" {
                client.send(SendRequest {
                    to: args.many("--to"),
                    group: args.get("--group"),
                    message,
                    tags: args.many("--tag"),
                    meta: metadata,
                    ttl_seconds,
                    idempotency_key: Some(key.clone()),
                })
            } else {
                client.reply(ReplyRequest {
                    message_id: args.words[1].clone(),
                    message,
                    tags: if args.flag("--clear-tags") || args.values.contains_key("--tag") {
                        Some(args.many("--tag"))
                    } else {
                        None
                    },
                    meta: metadata,
                    ttl_seconds,
                    idempotency_key: Some(key.clone()),
                })
            };
            let message = result.with_context(|| {
                format!(
                    "send outcome unconfirmed; retry the same request with --idempotency-key {key}"
                )
            })?;
            Ok(json!({"persisted":true,"id":message.id,"idempotency_key":key,"message":message}))
        }
        "message" => {
            args.validate(&[], &[])?;
            if args.words.len() < 3 || args.words[1] != "get" {
                bail!("expected message get ID [ID ...]");
            }
            Ok(serde_json::to_value(query::messages_get(
                &client.root,
                &client.state,
                &client.agent,
                &args.words[2..],
            )?)?)
        }
        "history" | "inbox" | "search" | "thread" => {
            args.validate(
                &[
                    "--group", "--sender", "--tag", "--after", "--before", "--limit", "--search",
                    "--thread",
                ],
                &[],
            )?;
            args.words(if verb == "search" || verb == "thread" {
                2
            } else {
                1
            })?;
            let thread = if verb == "thread" {
                let lookup = query::messages_get(
                    &client.root,
                    &client.state,
                    &client.agent,
                    &args.words[1..2],
                )?;
                Some(
                    lookup
                        .messages
                        .first()
                        .map(|m| m.thread.clone())
                        .unwrap_or_else(|| args.words[1].clone()),
                )
            } else {
                args.get("--thread")
            };
            let query = query::HistoryQuery {
                thread,
                search: if verb == "search" {
                    Some(args.words[1].clone())
                } else {
                    args.get("--search")
                },
                group: args.get("--group"),
                sender: args.get("--sender"),
                tags: args.many("--tag"),
                after: args.get("--after"),
                before: args.get("--before"),
                limit: args
                    .get("--limit")
                    .map(|value| value.parse::<usize>())
                    .transpose()
                    .context("--limit must be an integer")?
                    .unwrap_or(50),
            };
            Ok(serde_json::to_value(query::history(
                &client.root,
                &client.state,
                &client.agent,
                &query,
            )?)?)
        }
        "agents" => {
            args.validate(&["--search", "--limit", "--after"], &["--online"])?;
            args.words(1)?;
            let rows = query::agents(
                &client.state,
                &query::AgentQuery {
                    project: args.get("--project"),
                    online: args.flag("--online").then_some(true),
                    search: args.get("--search"),
                    limit: limit(args)?,
                    after: args.get("--after"),
                },
            )?;
            Ok(serde_json::to_value(rows)?)
        }
        "groups" => {
            args.validate(&["--search", "--limit", "--after"], &["--joined"])?;
            args.words(1)?;
            Ok(serde_json::to_value(query::groups(
                &client.state,
                &client.agent,
                &query::GroupQuery {
                    joined: args.flag("--joined"),
                    search: args.get("--search"),
                    limit: limit(args)?,
                    after: args.get("--after"),
                },
            )?)?)
        }
        "group" => {
            args.validate(&[], &[])?;
            args.words(3)?;
            let version_root = client.root.join("v1");
            match args.words[1].as_str() {
                "create" => Ok(serde_json::to_value(storage::create_group(
                    &version_root,
                    &args.words[2],
                    &client.agent.id,
                )?)?),
                "join" => Ok(serde_json::to_value(storage::join_group(
                    &version_root,
                    &args.words[2],
                    &client.agent.id,
                )?)?),
                _ => bail!("expected group create NAME or group join NAME"),
            }
        }
        "identity" | "tags" | "ping" | "status" | "doctor" => {
            args.validate(&[], &[])?;
            args.words(1)?;
            match verb {
                "identity" => Ok(json!({"agent":client.agent,"root":client.root})),
                "tags" => Ok(json!({"tags":routed::tags(&client.root)?})),
                "ping" => Ok(serde_json::to_value(storage::touch_presence(
                    &client.root.join("v1"),
                    &client.agent.id,
                )?)?),
                "status" => client.status(),
                _ => client.doctor(),
            }
        }
        _ => bail!("unknown command {verb}; use zboard --help"),
    }
}

pub fn help() -> &'static str {
    "Zboard one-shot commands (JSON responses; no listener checkpoint is changed):\n\
     message get ID [ID ...]\n\
     send (--to AGENT [--to AGENT ...] | --group GROUP) --message TEXT\n\
     reply ID --message TEXT [--clear-tags]\n\
       send/reply: [--tag TAG ...] [--meta-json JSON | --meta-file PATH]\n\
                   [--ttl-seconds N] [--idempotency-key ULID]\n\
     history | inbox | search TEXT | thread ID\n\
       [--group GROUP] [--sender AGENT] [--tag TAG ...] [--after ID] [--before ID]\n\
       [--limit N]\n\
     agents [--project PROJECT] [--online] [--search TEXT]\n\
     groups [--joined] [--search TEXT]\n\
       agents/groups: [--limit N] [--after ID_OR_NAME]\n\
     tags | identity | ping | status | doctor\n\
     watch [--after ID] [--notify-only] [--poll-seconds N]\n\
     group create NAME | group join NAME\n\
     id new\n\
     Common: --root PATH --session ID --identity-project PROJECT --path PATH\n\
             --request-id CORRELATION\n\
     Identity defaults to the session environment and existing registration.\n\
     Run identity once to register a new session. Read commands require registration.\n\
     --project also selects identity except on agents, where it filters results.\n\
     Read cursors are explicit, exclusive ULIDs; inbox does not acknowledge the wake listener.\n\
     watch emits JSONL, starts with new arrivals by default, and reconciles a 120s late-arrival window.\n\
     Retain the idempotency key when retrying sends with an uncertain outcome."
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Result<Arguments> {
        Arguments::parse(args.iter().map(|s| (*s).to_owned()))
    }
    #[test]
    fn repeated_tags_and_recipients_are_retained() {
        let args = parse(&[
            "send",
            "--to",
            "one",
            "--to=two",
            "--message",
            "hello world",
            "--tag",
            "build",
            "--tag",
            "release",
        ])
        .unwrap();
        assert_eq!(args.many("--to"), ["one", "two"]);
        assert_eq!(args.many("--tag"), ["build", "release"]);
        assert_eq!(args.get("--message").unwrap(), "hello world");
    }
    #[test]
    fn rejects_missing_duplicate_and_unknown_options() {
        assert!(parse(&["send", "--message"]).is_err());
        assert!(parse(&["send", "--message", "--to", "a"]).is_err());
        assert!(parse(&["history", "--limit", "2", "--limit", "3"]).is_err());
        assert!(
            parse(&["history", "--typo", "x"])
                .unwrap()
                .validate(&[], &[])
                .is_err()
        );
    }
    #[test]
    fn structured_meta_does_not_require_escaped_message_payload() {
        let args = parse(&[
            "send",
            "--meta-json",
            r#"{"schema":"job","argv":["cargo","test"]}"#,
        ])
        .unwrap();
        assert_eq!(meta(&args).unwrap().unwrap()["argv"][1], "test");
        assert!(
            meta(&parse(&["send", "--meta-json", "{}", "--meta-file", "unused"]).unwrap()).is_err()
        );
    }

    #[test]
    fn one_shot_identity_send_lookup_and_retry() {
        let root = tempfile::tempdir().unwrap();
        let invoke = |parts: &[&str]| {
            let mut argv: Vec<String> = parts.iter().map(|s| (*s).to_owned()).collect();
            argv.extend([
                "--root".into(),
                root.path().to_string_lossy().into_owned(),
                "--session".into(),
                "cli-unit-session".into(),
                "--identity-project".into(),
                "cli-test".into(),
            ]);
            execute(&Arguments::parse(argv.into_iter()).unwrap())
        };
        invoke(&["identity"]).unwrap();
        let key = ulid::Ulid::new().to_string();
        let first = invoke(&[
            "send",
            "--group",
            "global",
            "--message",
            "hello",
            "--idempotency-key",
            &key,
        ])
        .unwrap();
        let retry = invoke(&[
            "send",
            "--group",
            "global",
            "--message",
            "hello",
            "--idempotency-key",
            &key,
        ])
        .unwrap();
        assert_eq!(first["id"], retry["id"]);
        assert_eq!(first["persisted"], true);
        let read = invoke(&["message", "get", &key]).unwrap();
        assert_eq!(read["messages"][0]["message"], "hello");
        let inbox = invoke(&["inbox", "--limit", "1"]).unwrap();
        assert_eq!(inbox["messages"].as_array().unwrap().len(), 1);
    }
}

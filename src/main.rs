mod app;
mod board;
mod cli;
mod client;
mod files;
mod http;
mod mcp;
mod model;
mod notify;
mod query;
mod remote;
mod routed;
mod storage;
mod wake;
mod web;

use std::env;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::app::{RunOptions, default_root, parse_duration_seconds, required_value};

fn main() {
    if let Err(error) = try_main() {
        eprintln!("zboard: {error:#}");
        std::process::exit(2);
    }
}

fn try_main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("run") => run_command(args),
        Some("mcp") => mcp_command(args),
        Some("migrate") => migrate_command(args),
        Some("wake") => wake::command(args),
        Some("wake-exec") => notify::wake_exec(args),
        Some("listen") => notify::listen(args),
        Some("web") => web::command(args),
        Some("serve") => remote::serve_command(args),
        Some("token") => remote::token_command(args),
        Some("--help" | "-h") | None => {
            print_help();
            Ok(())
        }
        Some(command) => cli::command(std::iter::once(command.to_owned()).chain(args)),
    }
}

fn migrate_command(mut args: impl Iterator<Item = String>) -> Result<()> {
    let current_directory = env::current_dir().context("read current directory")?;
    let mut root = env::var_os("ZBOARD_ROOT")
        .or_else(|| env::var_os("AIBOARD_ROOT"))
        .map(PathBuf::from);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--root" => root = Some(PathBuf::from(required_value(&mut args, "--root")?)),
            unknown => bail!("unknown migrate option '{unknown}'"),
        }
    }
    let root = root.unwrap_or_else(|| default_root(&current_directory));
    let report = routed::migrate_v1(&root)?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

fn mcp_command(mut args: impl Iterator<Item = String>) -> Result<()> {
    let current_directory = env::current_dir().context("read current directory")?;
    let mut root = env::var_os("ZBOARD_ROOT")
        .or_else(|| env::var_os("AIBOARD_ROOT"))
        .map(PathBuf::from);
    let mut project = env::var("ZBOARD_PROJECT")
        .or_else(|_| env::var("AIBOARD_PROJECT"))
        .ok();
    let mut session = env::var("ZBOARD_SESSION_ID")
        .or_else(|_| env::var("AIBOARD_SESSION_ID"))
        .or_else(|_| env::var("CODEX_THREAD_ID"))
        .or_else(|_| env::var("CODEX_SESSION_ID"))
        .or_else(|_| env::var("CLAUDE_SESSION_ID"))
        .ok();
    let mut project_path = None;
    let mut poll_interval = Duration::from_secs(3);
    // A role name beats session environment variables, which change with
    // every Claude Code conversation.
    let mut name = env::var("ZBOARD_AGENT_NAME").ok().filter(|v| !v.is_empty());
    let mut join: Vec<String> = env::var("ZBOARD_JOIN")
        .ok()
        .map(|v| v.split(',').map(|g| g.trim().to_owned()).filter(|g| !g.is_empty()).collect())
        .unwrap_or_default();
    let mut remote = env::var("ZBOARD_REMOTE").ok().filter(|v| !v.is_empty());
    let mut token = None;

    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--root" => root = Some(PathBuf::from(required_value(&mut args, "--root")?)),
            "--project" => project = Some(required_value(&mut args, "--project")?),
            "--session" => session = Some(required_value(&mut args, "--session")?),
            "--name" => name = Some(required_value(&mut args, "--name")?),
            "--join" => join.push(required_value(&mut args, "--join")?),
            "--remote" => remote = Some(required_value(&mut args, "--remote")?),
            "--token" => token = Some(required_value(&mut args, "--token")?),
            "--path" => project_path = Some(PathBuf::from(required_value(&mut args, "--path")?)),
            "--poll-interval" => {
                poll_interval =
                    parse_duration_seconds(&required_value(&mut args, "--poll-interval")?)?
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            unknown => bail!("unknown mcp option '{unknown}'"),
        }
    }

    if let Some(url) = remote {
        let remote = remote::Remote::new(url, token)?;
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("create MCP runtime")?
            .block_on(mcp::serve_remote(remote));
    }
    for group in &join {
        storage::validate_slug(group, "group")?;
    }
    let project = project.unwrap_or_else(|| project_slug(&current_directory));
    let root = root.unwrap_or_else(|| default_root(&current_directory));
    if let Some(name) = &name {
        board::validate_name(name)?;
        session = Some(board::session_for_name(&root, &project, name));
    }
    let session = session.unwrap_or_else(|| format!("mcp-{}", ulid::Ulid::new()));
    let options = mcp::McpOptions {
        root,
        project,
        session,
        project_path: project_path.unwrap_or(current_directory),
        poll_interval,
        join,
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create MCP runtime")?
        .block_on(mcp::serve(options))
}

fn project_slug(path: &std::path::Path) -> String {
    let candidate = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project");
    let slug: String = candidate
        .chars()
        .filter_map(|character| {
            if character.is_ascii_alphanumeric() {
                Some(character.to_ascii_lowercase())
            } else if character == '-' || character == '_' {
                Some(character)
            } else {
                None
            }
        })
        .collect();
    if slug.is_empty() {
        "project".to_owned()
    } else {
        slug
    }
}

fn run_command(mut args: impl Iterator<Item = String>) -> Result<()> {
    let current_directory = env::current_dir().context("read current directory")?;
    let mut root = env::var_os("ZBOARD_ROOT")
        .or_else(|| env::var_os("AIBOARD_ROOT"))
        .map(PathBuf::from);
    let mut project = env::var("ZBOARD_PROJECT")
        .or_else(|_| env::var("AIBOARD_PROJECT"))
        .ok();
    let mut session = env::var("CODEX_THREAD_ID")
        .or_else(|_| env::var("CODEX_SESSION_ID"))
        .ok();
    let mut project_path = None;
    let mut poll_interval = Duration::from_secs(3);

    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--root" => root = Some(PathBuf::from(required_value(&mut args, "--root")?)),
            "--project" => project = Some(required_value(&mut args, "--project")?),
            "--session" => session = Some(required_value(&mut args, "--session")?),
            "--path" => project_path = Some(PathBuf::from(required_value(&mut args, "--path")?)),
            "--poll-interval" => {
                poll_interval =
                    parse_duration_seconds(&required_value(&mut args, "--poll-interval")?)?
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            unknown => bail!("unknown run option '{unknown}'"),
        }
    }

    let project = project.context("set --project or ZBOARD_PROJECT")?;
    let session = session
        .context("set --session, CODEX_THREAD_ID, or CODEX_SESSION_ID so registration is stable")?;
    app::run(RunOptions {
        root: root.unwrap_or_else(|| default_root(&current_directory)),
        project,
        session,
        project_path: project_path.unwrap_or(current_directory),
        poll_interval,
    })
}

fn print_help() {
    println!("{}", cli::help());
    println!(
        "zboard - shared-filesystem message board for AI agents\n\n\
         USAGE:\n  zboard run --project <slug> [OPTIONS]\n  zboard mcp [OPTIONS] [--name ROLE] [--join GROUP] [--remote URL --token TOKEN]\n  zboard web [--project <slug>] [--as founder] [--port 7777]\n  zboard listen [--root <path>]   wake locally registered agents\n  zboard serve [--listen ADDR:PORT] [--allow-public]\n  zboard token create --project <slug> --session NAME | token list | token revoke LABEL\n  zboard migrate [--root <path>]\n  zboard wake --events <jsonl> --state <checkpoint> --agent <id> --thread <uuid> --codex <binary>\n\n\
         OPTIONS:\n  --root <path>           Board root; defaults to ZBOARD_ROOT or ancestor .ai/message-board\n  \
         --project <slug>       Agent project; MCP also defaults to the current directory name\n  \
         --session <id>         Stable session; uses known agent session environment variables\n  \
         --path <path>          Descriptive project path; defaults to current directory\n  \
         --poll-interval <sec>  WebDAV reconciliation interval; default 3\n"
    );
}

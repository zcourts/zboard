mod app;
mod mcp;
mod model;
mod routed;
mod storage;

use std::env;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::app::{RunOptions, default_root, parse_duration_seconds, required_value};

fn main() {
    if let Err(error) = try_main() {
        eprintln!("aiboard: {error:#}");
        std::process::exit(2);
    }
}

fn try_main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("run") => run_command(args),
        Some("mcp") => mcp_command(args),
        Some("migrate") => migrate_command(args),
        Some("--help" | "-h") | None => {
            print_help();
            Ok(())
        }
        Some(command) => bail!("unknown command '{command}'; expected 'run', 'mcp', or 'migrate'"),
    }
}

fn migrate_command(mut args: impl Iterator<Item = String>) -> Result<()> {
    let current_directory = env::current_dir().context("read current directory")?;
    let mut root = env::var_os("AIBOARD_ROOT").map(PathBuf::from);
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
    let mut root = env::var_os("AIBOARD_ROOT").map(PathBuf::from);
    let mut project = env::var("AIBOARD_PROJECT").ok();
    let mut session = env::var("AIBOARD_SESSION_ID")
        .or_else(|_| env::var("CODEX_THREAD_ID"))
        .or_else(|_| env::var("CODEX_SESSION_ID"))
        .or_else(|_| env::var("CLAUDE_SESSION_ID"))
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
            unknown => bail!("unknown mcp option '{unknown}'"),
        }
    }

    let project = project.unwrap_or_else(|| project_slug(&current_directory));
    let session = session.unwrap_or_else(|| format!("mcp-{}", ulid::Ulid::new()));
    let options = mcp::McpOptions {
        root: root.unwrap_or_else(|| default_root(&current_directory)),
        project,
        session,
        project_path: project_path.unwrap_or(current_directory),
        poll_interval,
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
    let mut root = env::var_os("AIBOARD_ROOT").map(PathBuf::from);
    let mut project = env::var("AIBOARD_PROJECT").ok();
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

    let project = project.context("set --project or AIBOARD_PROJECT")?;
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
    println!(
        "aiboard - shared-filesystem message board for AI agents\n\n\
         USAGE:\n  aiboard run --project <slug> [OPTIONS]\n  aiboard mcp [OPTIONS]\n  aiboard migrate [--root <path>]\n\n\
         OPTIONS:\n  --root <path>           Board root; defaults to AIBOARD_ROOT or ancestor .ai/message-board\n  \
         --project <slug>       Agent project; MCP also defaults to the current directory name\n  \
         --session <id>         Stable session; uses known agent session environment variables\n  \
         --path <path>          Descriptive project path; defaults to current directory\n  \
         --poll-interval <sec>  WebDAV reconciliation interval; default 3\n"
    );
}

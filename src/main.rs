//! jj-collect — record each Claude session's edits and construct a per-agent jj
//! change from them, so agents sharing one working copy each get their own commit.
//!
//! Two halves, deliberately decoupled:
//!   * **record** — `jj collect` marks a session; a hook then snapshots `@` around
//!     each edit (via jj itself) and appends a cheap `(pre, post, files)` record.
//!     No stack mutation, no lock — so any number of agents record concurrently.
//!   * **construct** — `jj collect --build` replays each session's records into an
//!     isolated change, single-threaded, letting jj's rebase do the content math.
//!
//! Usage:
//!   jj collect [-m MSG]            start recording this session's edits
//!   jj collect --build [--agent A] build the change from what was recorded
//!   jj collect --build --all       build every collecting session's change
//!   jj-collect --install/--uninstall/--hook   plumbing

mod construct;
mod hook;
mod identity;
mod install;
mod jj;
mod lock;
mod paths;
mod store;

use clap::Parser;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::exit;

use identity::{from_cli, ENV_VAR};
use jj::Jj;
use paths::{ensure_data_dir, find_repo_root};

#[derive(Parser)]
#[command(
    name = "jj-collect",
    about = "Record this Claude session's edits and build them into their own jj change.",
    version
)]
struct Cli {
    /// Hook entry point (reads hook JSON on stdin; used in settings.json).
    #[arg(long, hide = true)]
    hook: bool,
    /// Register the hooks in Claude settings, plus `jj collect` as a jj alias.
    #[arg(long, conflicts_with = "uninstall")]
    install: bool,
    /// Remove the hooks and the jj alias.
    #[arg(long)]
    uninstall: bool,
    /// With --install/--uninstall: target this repo's settings, not the global file.
    #[arg(long)]
    project: bool,
    /// Construct the collected change(s) from the recorded edits.
    #[arg(long)]
    build: bool,
    /// With --build: build every collecting session, not just this one.
    #[arg(long)]
    all: bool,
    /// Description for the change (recorded now, applied at --build).
    #[arg(short = 'm', long)]
    message: Option<String>,
    /// Session to act as (default: $JJ_COLLECT_AGENT, set by the SessionStart hook).
    #[arg(long)]
    agent: Option<String>,
}

fn main() {
    let cli = Cli::parse();
    let code = if cli.hook {
        hook::run_hook()
    } else if cli.install {
        cmd_install(cli.project)
    } else if cli.uninstall {
        cmd_uninstall(cli.project)
    } else if cli.build {
        cmd_build(cli.agent, cli.all)
    } else {
        cmd_collect(cli.message, cli.agent)
    };
    exit(code);
}

// --------------------------------------------------------------------------- //
// record
// --------------------------------------------------------------------------- //
fn cmd_collect(message: Option<String>, agent_opt: Option<String>) -> i32 {
    let root = match repo_root() {
        Some(r) => r,
        None => {
            err("Not inside a jj repo.");
            return 2;
        }
    };
    let agent = match resolve_agent(agent_opt) {
        Some(a) => a,
        None => return 2,
    };
    let base_dir = match ensure_data_dir(&root) {
        Ok(b) => b,
        Err(e) => {
            err(&format!("could not create data dir: {e}"));
            return 1;
        }
    };
    let jj = Jj::new(&root);
    // The construction floor is the working copy as it is right now, before this
    // session edits — recorded once, shared by every session.
    if let Some(c) = jj.snapshot_commit() {
        store::base_set_once(&base_dir, &c);
    }
    store::session_start(&base_dir, &agent, message.as_deref());
    println!(
        "{} session {agent}: recording edits — run `jj collect --build` when done",
        green("✓")
    );
    0
}

// --------------------------------------------------------------------------- //
// construct
// --------------------------------------------------------------------------- //
fn cmd_build(agent_opt: Option<String>, all: bool) -> i32 {
    let root = match repo_root() {
        Some(r) => r,
        None => {
            err("Not inside a jj repo.");
            return 2;
        }
    };
    let base_dir = match ensure_data_dir(&root) {
        Ok(b) => b,
        Err(e) => {
            err(&format!("data dir: {e}"));
            return 1;
        }
    };
    let base = match store::base_get(&base_dir) {
        Some(b) => b,
        None => {
            println!("Nothing collected yet.");
            return 0;
        }
    };
    let jj = Jj::new(&root);
    // One builder at a time (construction moves the working copy around).
    let _guard = lock::Guard::new(&base_dir, "build");

    let targets: Vec<(String, Option<String>)> = if all {
        store::sessions(&base_dir)
            .into_iter()
            .map(|s| {
                let m = store::session_message(&base_dir, &s);
                (s, m)
            })
            .collect()
    } else {
        let agent = match resolve_agent(agent_opt) {
            Some(a) => a,
            None => return 2,
        };
        let m = store::session_message(&base_dir, &agent);
        vec![(agent, m)]
    };

    let events = store::events(&base_dir);
    let orig = jj.snapshot_commit(); // the live, all-agents working copy
    let built = construct::build_all(&jj, &base, &targets, &events, orig.as_deref());

    if built.is_empty() {
        println!("Nothing to build for the requested session(s).");
        return 0;
    }
    for b in built {
        if b.conflict {
            println!(
                "{} session {} → change {} — {} file(s) (inspect: jj show {})",
                yellow("⚠ built WITH CONFLICTS:"),
                b.session,
                b.change_id,
                b.files,
                b.change_id
            );
        } else {
            println!(
                "{} session {} → change {} on base — {} file(s) (inspect: jj show {})",
                green("✓ built"),
                b.session,
                b.change_id,
                b.files,
                b.change_id
            );
        }
    }
    0
}

// --------------------------------------------------------------------------- //
// install / uninstall
// --------------------------------------------------------------------------- //
fn cmd_install(project: bool) -> i32 {
    let target = default_settings_path(project);
    let full = default_hook_command();
    match install::merge_settings(&target, &full) {
        Ok(added) => {
            let verb = if added { "Installed" } else { "Already present —" };
            let scope = if project { "project" } else { "global" };
            println!("{} hooks ({scope}) → {}", green(verb), target.display());
            println!("  command:  {full}");
            if set_jj_alias() {
                println!("  alias:    `jj collect …` (jj user config → aliases.collect)");
            }
            println!("\nAgents: run `jj collect -m \"<task>\"` before editing, `jj collect --build` when done.");
            0
        }
        Err(e) => {
            err(&format!("could not write {}: {e}", target.display()));
            1
        }
    }
}

fn cmd_uninstall(project: bool) -> i32 {
    let target = default_settings_path(project);
    unset_jj_alias();
    match install::remove_settings(&target) {
        Ok(n) => {
            println!(
                "{} removed {n} hook entr{} from {}",
                green("✓"),
                if n == 1 { "y" } else { "ies" },
                target.display()
            );
            0
        }
        Err(e) => {
            err(&format!("could not write {}: {e}", target.display()));
            1
        }
    }
}

fn set_jj_alias() -> bool {
    let exe = current_exe();
    let value = serde_json::to_string(&["util", "exec", "--", exe.as_str()]).unwrap();
    std::process::Command::new("jj")
        .args(["config", "set", "--user", "aliases.collect", &value])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn unset_jj_alias() {
    let _ = std::process::Command::new("jj")
        .args(["config", "unset", "--user", "aliases.collect"])
        .status();
}

// --------------------------------------------------------------------------- //
// helpers
// --------------------------------------------------------------------------- //
fn repo_root() -> Option<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    find_repo_root(&cwd)
}

fn resolve_agent(explicit: Option<String>) -> Option<String> {
    match explicit.or_else(|| from_cli(None)) {
        Some(a) => Some(a),
        None => {
            err(&format!(
                "Could not determine which session you are. Pass --agent <id> or set ${ENV_VAR}.\n\
                 (The SessionStart hook sets it automatically once `jj-collect --install` has run.)"
            ));
            None
        }
    }
}

fn current_exe() -> String {
    std::env::current_exe().ok().map(|p| p.display().to_string()).unwrap_or_else(|| "jj-collect".to_string())
}

fn default_settings_path(project: bool) -> PathBuf {
    if project {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        find_repo_root(&cwd).unwrap_or(cwd).join(".claude").join("settings.json")
    } else {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".claude").join("settings.json")
    }
}

fn default_hook_command() -> String {
    format!("\"{}\" --hook", current_exe())
}

// --- tiny terminal styling (no external crates) ---
fn green(s: &str) -> String {
    paint(s, "32")
}
fn yellow(s: &str) -> String {
    paint(s, "33")
}
fn paint(s: &str, code: &str) -> String {
    if std::io::stdout().is_terminal() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}
fn err(msg: &str) {
    if std::io::stderr().is_terminal() {
        eprintln!("\x1b[31m{msg}\x1b[0m");
    } else {
        eprintln!("{msg}");
    }
}

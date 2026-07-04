//! jj-extract — agents share one jj working copy; each edit is recorded into
//! jj's evolog (tagged with the acting session), and `jj extract` pulls a
//! session's edits into their own jj change — isolated at line granularity, with
//! jj's rebase doing the content math.
//!
//! Recording is automatic (a hook tags every edit's snapshot); there's nothing
//! to start. `jj extract` is a `jj new`-style wrapper that mints a change and
//! composes the acting session's tagged evolutions into it.
//!
//! Usage:
//!   jj extract [-m MSG]           pull my edits into their own change
//!   jj extract --all              build a change for every session in the evolog
//!   jj-extract --install/--uninstall/--hook   plumbing

mod construct;
mod hook;
mod identity;
mod install;
mod jj;
mod lock;
mod paths;

use clap::Parser;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::exit;

use identity::{from_cli, ENV_VAR};
use jj::Jj;
use paths::find_repo_root;

#[derive(Parser)]
#[command(
    name = "jj-extract",
    about = "Pull this Claude session's recorded edits into their own jj change.",
    version
)]
struct Cli {
    /// Hook entry point (reads hook JSON on stdin; used in settings.json).
    #[arg(long, hide = true)]
    hook: bool,
    /// Register the hooks in Claude settings, plus `jj extract` as a jj alias.
    #[arg(long, conflicts_with = "uninstall")]
    install: bool,
    /// Remove the hooks and the jj alias.
    #[arg(long)]
    uninstall: bool,
    /// With --install/--uninstall: target this repo's settings, not the global file.
    #[arg(long)]
    project: bool,
    /// Extract a change for every session found in the evolog, not just this one.
    #[arg(long)]
    all: bool,
    /// Description for the extracted change.
    #[arg(short = 'm', long)]
    message: Option<String>,
    /// Session to extract (default: $JJ_EXTRACT_AGENT, set by the SessionStart hook).
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
    } else {
        cmd_extract(cli.message, cli.agent, cli.all)
    };
    exit(code);
}

// --------------------------------------------------------------------------- //
// extract — the one command agents use
// --------------------------------------------------------------------------- //
fn cmd_extract(message: Option<String>, agent_opt: Option<String>, all: bool) -> i32 {
    let root = match repo_root() {
        Some(r) => r,
        None => {
            err("Not inside a jj repo.");
            return 2;
        }
    };
    let jj = Jj::new(&root);
    // Hold the edit lock (in the repo's `.jj/`) while we read the evolog and
    // construct: it stops a concurrent hook from snapshotting `@` into the
    // build's intermediate states.
    let _guard = lock::Guard::new(&root.join(".jj"), "extract");

    let evolog = jj.evolog();
    // The extracted change branches from `@`'s parent — the commit the working
    // copy is built on, i.e. the state before any editing. So the change holds
    // only the agent's own edits, not other agents' work nor the live `@`.
    let base = match jj.change_id("@-") {
        Some(b) => b,
        None => {
            println!("Nothing recorded yet.");
            return 0;
        }
    };

    let targets: Vec<(String, Option<String>)> = if all {
        construct::agents_in(&evolog).into_iter().map(|a| (a, None)).collect()
    } else {
        let agent = match resolve_agent(agent_opt) {
            Some(a) => a,
            None => return 2,
        };
        vec![(agent, message)]
    };

    // Remember the live working copy so we can restore `@` after building moves it.
    let orig = jj.change_id("@");
    let mut built = vec![];
    for (agent, msg) in &targets {
        if let Some(b) = construct::build_one(&jj, &base, agent, &evolog, msg.as_deref()) {
            built.push(b);
        }
    }
    if let Some(o) = &orig {
        let _ = jj.edit(o);
    }

    if built.is_empty() {
        println!("Nothing to extract for the requested session(s).");
        return 0;
    }
    for b in built {
        if b.conflict {
            println!(
                "{} session {} → change {} (inspect: jj show {})",
                yellow("⚠ extracted WITH CONFLICTS:"),
                b.session,
                b.change_id,
                b.change_id
            );
        } else {
            println!(
                "{} session {} → change {} on base (inspect: jj show {})",
                green("✓ extracted"),
                b.session,
                b.change_id,
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
                println!("  alias:    `jj extract …` (jj user config → aliases.extract)");
            }
            println!("\nRecording is automatic. Agents run `jj extract -m \"<task>\"` to pull their change.");
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

/// Register `jj extract …` as a jj user alias that shells out to this binary via
/// `jj util exec` (the documented pattern for external jj subcommands).
fn set_jj_alias() -> bool {
    let exe = current_exe();
    let value = serde_json::to_string(&["util", "exec", "--", exe.as_str()]).unwrap();
    std::process::Command::new("jj")
        .args(["config", "set", "--user", "aliases.extract", &value])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn unset_jj_alias() {
    let _ = std::process::Command::new("jj")
        .args(["config", "unset", "--user", "aliases.extract"])
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
                 (The SessionStart hook sets it automatically once `jj-extract --install` has run.)"
            ));
            None
        }
    }
}

fn current_exe() -> String {
    std::env::current_exe().ok().map(|p| p.display().to_string()).unwrap_or_else(|| "jj-extract".to_string())
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

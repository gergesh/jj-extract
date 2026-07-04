//! jj-collect — run it instead of `jj new` to open a change that *this Claude
//! session's* edits automatically collect into.
//!
//! Multiple agents share one jj working copy. Each runs `jj collect` once; from
//! then on a `PostToolUse` hook squashes that session's edits into its change —
//! line-level, with jj's diff/rebase engine doing all the content math. Every
//! agent's work ends up isolated in its own commit. Installed as `jj-collect`,
//! it is also reachable as the native subcommand `jj collect …`.
//!
//! Usage:
//!   jj collect [-m MSG]        open a fresh change and collect my edits into it
//!   jj collect --to <rev>      collect my edits into an existing change instead
//!   jj-collect --install       register the hook + the `jj collect` alias
//!   jj-collect --uninstall     remove them
//!   jj-collect --hook          hook entry point (used in settings.json)

mod hook;
mod identity;
mod install;
mod jj;
mod lock;
mod paths;
mod stack;
mod state;

use clap::Parser;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::exit;

use identity::{from_cli, ENV_VAR};
use jj::Jj;
use lock::RepoLock;
use paths::{data_dir_for_root, find_repo_root};
use state::State;

#[derive(Parser)]
#[command(
    name = "jj-collect",
    about = "Open a jj change that this Claude session's edits collect into (like `jj new`).",
    version
)]
struct Cli {
    /// Hook entry point (reads hook JSON on stdin; used in settings.json).
    #[arg(long, hide = true)]
    hook: bool,
    /// Register the hook in Claude settings, plus `jj collect` as a jj alias.
    #[arg(long, conflicts_with = "uninstall")]
    install: bool,
    /// Remove the hook and the jj alias.
    #[arg(long)]
    uninstall: bool,
    /// With --install/--uninstall: target this repo's settings, not the global file.
    #[arg(long)]
    project: bool,
    /// Description for the new change (like `jj new -m`).
    #[arg(short = 'm', long)]
    message: Option<String>,
    /// Collect into an existing change/revision instead of opening a new one.
    #[arg(long, value_name = "REV")]
    to: Option<String>,
    /// Session to bind (default: $JJ_COLLECT_AGENT, set by the SessionStart hook).
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
        cmd_collect(cli.message, cli.to, cli.agent)
    };
    exit(code);
}

// --------------------------------------------------------------------------- //
// collect — the one command agents use
// --------------------------------------------------------------------------- //
fn cmd_collect(message: Option<String>, to: Option<String>, agent_opt: Option<String>) -> i32 {
    let (root, base_dir) = match repo() {
        Some(rb) => rb,
        None => {
            err("Not inside a jj repo.");
            return 2;
        }
    };
    let agent = match agent_opt.or_else(|| from_cli(None)) {
        Some(a) => a,
        None => {
            err(&format!(
                "Could not determine which session you are. Pass --agent <id> or set ${ENV_VAR}.\n\
                 (The SessionStart hook sets it automatically once `jj-collect --install` has run.)"
            ));
            return 2;
        }
    };

    let jj = Jj::new(&root);
    // Serialize against concurrent hooks/collects — both mutate the stack.
    let _guard = RepoLock::acquire(&base_dir).ok();
    let mut state = State::load(&base_dir);

    let change = match &to {
        Some(rev) => match jj.change_id(rev) {
            Some(c) => {
                if let Some(m) = &message {
                    jj.describe(&c, m);
                }
                c
            }
            None => {
                err(&format!("revision {rev:?} not found."));
                return 1;
            }
        },
        None => {
            // Establish the floor first so the change is minted above it (and
            // above the holding change the hook inserts just above the base).
            stack::ensure_base(&jj, &mut state, &base_dir);
            match jj.mint_change(message.as_deref()) {
                Some(c) => c,
                None => {
                    err("could not open a new change.");
                    return 1;
                }
            }
        }
    };

    state.bindings.insert(agent.clone(), change.clone());
    if let Err(e) = state.save(&base_dir) {
        err(&format!("save: {e}"));
        return 1;
    }

    let how = if to.is_some() { "into existing change" } else { "into new change" };
    println!("{} {agent}: collecting edits {how} {change}", green("✓"));
    0
}

// --------------------------------------------------------------------------- //
// install / uninstall — plumbing
// --------------------------------------------------------------------------- //
fn cmd_install(project: bool) -> i32 {
    let target = default_settings_path(project);
    let full = default_hook_command();
    match install::merge_settings(&target, &full) {
        Ok(added) => {
            let verb = if added { "Installed" } else { "Already present —" };
            let scope = if project { "project" } else { "global" };
            println!("{} hook ({scope}) → {}", green(verb), target.display());
            println!("  command:  {full}");
            if set_jj_alias() {
                println!("  alias:    `jj collect …` (jj user config → aliases.collect)");
            }
            println!("\nAgents: run `jj collect -m \"<what you're doing>\"` before editing.");
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

/// Register `jj collect …` as a jj user alias that shells out to this binary via
/// `jj util exec` (the documented pattern for external jj subcommands). Returns
/// true on success; best-effort, so a missing jj is not fatal to `--install`.
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
fn repo() -> Option<(PathBuf, PathBuf)> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = find_repo_root(&cwd)?;
    let base = data_dir_for_root(&root);
    Some((root, base))
}

fn current_exe() -> String {
    std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "jj-collect".to_string())
}

fn default_settings_path(project: bool) -> PathBuf {
    if project {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let root = find_repo_root(&cwd).unwrap_or(cwd);
        root.join(".claude").join("settings.json")
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".claude")
            .join("settings.json")
    }
}

fn default_hook_command() -> String {
    format!("\"{}\" --hook", current_exe())
}

// --- tiny terminal styling (no external crates) ---
fn green(s: &str) -> String {
    if std::io::stdout().is_terminal() {
        format!("\x1b[32m{s}\x1b[0m")
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

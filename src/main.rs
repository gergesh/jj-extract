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
//!   jj-extract --install/--uninstall/--hook   agent integration plumbing

mod construct;
mod hook;
mod identity;
mod install;
mod jj;
mod jj_config;
mod lock;
mod new_files;
mod paths;
mod shell;

use clap::{ArgGroup, Parser};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::exit;

use identity::{from_cli, ENV_VAR};
use jj::Jj;
use paths::find_repo_root;

#[derive(Parser)]
#[command(
    name = "jj-extract",
    about = "Pull this agent session's recorded edits into their own jj change.",
    version,
    group(ArgGroup::new("management").args(["install", "uninstall"]).multiple(false))
)]
struct Cli {
    /// Hook entry point (reads hook JSON on stdin; used by Claude Code and Codex).
    #[arg(
        long,
        hide = true,
        conflicts_with_all = ["install", "uninstall", "project", "all", "agent"]
    )]
    hook: bool,
    /// Register Claude Code and Codex hooks, plus `jj extract` as a jj alias.
    #[arg(long, conflicts_with_all = ["all", "agent"])]
    install: bool,
    /// Remove the hooks and the jj alias.
    #[arg(long, conflicts_with_all = ["all", "agent"])]
    uninstall: bool,
    /// With --install/--uninstall: target this repo's hook configs, not global ones.
    #[arg(long, requires = "management")]
    project: bool,
    /// Extract a change for every session found in the evolog, not just this one.
    #[arg(long, conflicts_with = "agent")]
    all: bool,
    /// Session to extract (default: the current agent's exported session identity).
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
        cmd_extract(cli.agent, cli.all)
    };
    exit(code);
}

// --------------------------------------------------------------------------- //
// extract — the one command agents use
// --------------------------------------------------------------------------- //
fn cmd_extract(agent_opt: Option<String>, all: bool) -> i32 {
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

    let evolog = match jj.evolog() {
        Ok(evolutions) => evolutions,
        Err(e) => {
            err(&e);
            return 1;
        }
    };
    if evolog.is_empty() {
        println!("Nothing recorded yet.");
        return 0;
    }

    let targets: Vec<String> = if all {
        construct::agents_in(&evolog)
    } else {
        let agent = match resolve_agent(agent_opt) {
            Some(a) => a,
            None => return 2,
        };
        vec![agent]
    };

    let built = match construct::extract(&root, &jj, &evolog, &targets) {
        Ok(built) => built,
        Err(e) => {
            err(&format!("Extraction stopped: {e}"));
            return 1;
        }
    };

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
            let verb = if b.updated {
                green("↻ updated")
            } else {
                green("✓ extracted")
            };
            println!(
                "{} session {} → change {} in stack (inspect: jj show {})",
                verb, b.session, b.change_id, b.change_id
            );
        }
    }
    0
}

// --------------------------------------------------------------------------- //
// install / uninstall
// --------------------------------------------------------------------------- //
fn cmd_install(project: bool) -> i32 {
    let targets = match default_settings_paths(project) {
        Ok(targets) => targets,
        Err(e) => {
            err(&e);
            return 2;
        }
    };
    let full = default_hook_command();
    // Validate every destination before changing either one, so a malformed
    // config cannot leave the two integrations half-installed.
    for target in &targets {
        if let Err(e) = install::validate_settings_file(&target.path) {
            err(&format!("could not use {}: {e}", target.path.display()));
            return 1;
        }
    }
    let scope = if project { "project" } else { "global" };
    for target in &targets {
        let result = match target.client {
            HookClient::Claude => install::merge_claude_settings(&target.path, &full),
            HookClient::Codex => install::merge_codex_settings(&target.path, &full),
        };
        let added = match result {
            Ok(added) => added,
            Err(e) => {
                err(&format!("could not write {}: {e}", target.path.display()));
                return 1;
            }
        };
        let verb = if added {
            "Installed"
        } else {
            "Already present —"
        };
        println!(
            "{} {} hooks ({scope}) → {}",
            green(verb),
            target.name,
            target.path.display()
        );
    }
    println!("  command:  {full}");
    match jj_config::install_alias(current_exe()) {
        Ok(path) => {
            println!(
                "  alias:    `jj extract …` (jj user config → {})",
                path.display()
            );
        }
        Err(e) => {
            err(&format!(
                "Hooks were installed, but the `jj extract` alias could not be configured: {e}"
            ));
            return 1;
        }
    }
    println!(
        "\nRecording is automatic. Agents run `jj extract` to pull their change, then\n\
         describe it with `jj describe`."
    );
    println!(
        "Restart active agent sessions. In Codex, open `/hooks` and trust the new hook definition."
    );
    0
}

fn cmd_uninstall(project: bool) -> i32 {
    let targets = match default_settings_paths(project) {
        Ok(targets) => targets,
        Err(e) => {
            err(&e);
            return 2;
        }
    };
    for target in &targets {
        if let Err(e) = install::validate_settings_file(&target.path) {
            err(&format!("could not use {}: {e}", target.path.display()));
            return 1;
        }
    }
    for target in &targets {
        match install::remove_settings(&target.path) {
            Ok(n) => println!(
                "{} removed {n} {} hook entr{} from {}",
                green("✓"),
                target.name,
                if n == 1 { "y" } else { "ies" },
                target.path.display()
            ),
            Err(e) => {
                err(&format!("could not write {}: {e}", target.path.display()));
                return 1;
            }
        }
    }
    match jj_config::uninstall_alias() {
        Ok(_) => 0,
        Err(e) => {
            err(&format!(
                "Hooks were removed, but the `jj extract` alias could not be removed: {e}"
            ));
            1
        }
    }
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
                 (Claude Code and Codex export it automatically after hooks are installed.)"
            ));
            None
        }
    }
}

fn current_exe() -> String {
    std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "jj-extract".to_string())
}

struct HookTarget {
    name: &'static str,
    path: PathBuf,
    client: HookClient,
}

#[derive(Clone, Copy)]
enum HookClient {
    Claude,
    Codex,
}

fn default_settings_paths(project: bool) -> Result<Vec<HookTarget>, String> {
    let (claude, codex) = if project {
        let cwd = std::env::current_dir()
            .map_err(|e| format!("could not read current directory: {e}"))?;
        let root = find_repo_root(&cwd)
            .ok_or_else(|| "--project must be run inside a jj repository".to_string())?;
        (
            root.join(".claude").join("settings.json"),
            root.join(".codex").join("hooks.json"),
        )
    } else {
        let home = dirs::home_dir().ok_or_else(|| {
            "could not determine the home directory for global settings".to_string()
        })?;
        let codex_home = std::env::var_os("CODEX_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        (
            home.join(".claude").join("settings.json"),
            codex_home.join("hooks.json"),
        )
    };
    Ok(vec![
        HookTarget {
            name: "Claude Code",
            path: claude,
            client: HookClient::Claude,
        },
        HookTarget {
            name: "Codex",
            path: codex,
            client: HookClient::Codex,
        },
    ])
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

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;

    #[test]
    fn rejects_options_that_would_be_silently_ignored() {
        for args in [
            vec!["jj-extract", "--project"],
            vec!["jj-extract", "--all", "--message", "gone"],
            vec!["jj-extract", "--all", "--agent", "ignored"],
            vec!["jj-extract", "--install", "--all"],
            vec!["jj-extract", "--hook", "--all"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }
}

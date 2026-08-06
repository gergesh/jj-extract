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
mod lock;
mod paths;

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
        conflicts_with_all = ["install", "uninstall", "project", "all", "message", "agent"]
    )]
    hook: bool,
    /// Register Claude Code and Codex hooks, plus `jj extract` as a jj alias.
    #[arg(long, conflicts_with_all = ["all", "message", "agent"])]
    install: bool,
    /// Remove the hooks and the jj alias.
    #[arg(long, conflicts_with_all = ["all", "message", "agent"])]
    uninstall: bool,
    /// With --install/--uninstall: target this repo's hook configs, not global ones.
    #[arg(long, requires = "management")]
    project: bool,
    /// Extract a change for every session found in the evolog, not just this one.
    #[arg(long, conflicts_with_all = ["message", "agent"])]
    all: bool,
    /// Description for the extracted change.
    #[arg(short = 'm', long)]
    message: Option<String>,
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

    let evolog = match jj.evolog() {
        Ok(evolutions) => evolutions,
        Err(e) => {
            err(&e);
            return 1;
        }
    };
    // Use the parent of @'s oldest recorded evolution as the stable base. After
    // an earlier extraction, @'s immediate parent is an extracted change; using
    // that would replay already-extracted edits onto themselves on the next run.
    let Some(first_evolution) = evolog.first() else {
        println!("Nothing recorded yet.");
        return 0;
    };
    let original_parent = format!("{}-", first_evolution.commit);
    let base = match jj.change_id(&original_parent) {
        Some(b) => b,
        None => {
            err("Could not resolve the parent of the first recorded evolution.");
            return 1;
        }
    };

    let targets: Vec<(String, Option<String>)> = if all {
        construct::agents_in(&evolog)
            .into_iter()
            .map(|a| (a, None))
            .collect()
    } else {
        let agent = match resolve_agent(agent_opt) {
            Some(a) => a,
            None => return 2,
        };
        vec![(agent, message)]
    };

    // Preserve the live working-copy change id while extracted changes are
    // rebuilt and inserted below it.
    let orig = match jj.change_id("@") {
        Some(change) => change,
        None => {
            err("Could not resolve the live working-copy change.");
            return 1;
        }
    };
    let mut built = vec![];
    let mut build_error = None;
    for (agent, msg) in &targets {
        match construct::build_one(&jj, &base, agent, &evolog, msg.as_deref()) {
            Ok(Some(mut b)) => {
                // Idempotency: if this session was already extracted, update that
                // change in place instead of leaving a duplicate behind.
                match construct::reconcile_idempotent(&jj, agent, &b.change_id, msg.as_deref()) {
                    Ok((id, updated)) => {
                        b.change_id = id;
                        b.updated = updated;
                        built.push(b);
                    }
                    Err(e) => {
                        build_error = Some(e);
                        break;
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                build_error = Some(e);
                break;
            }
        }
    }
    if build_error.is_none() && !built.is_empty() {
        if let Err(e) = construct::stack_extractions(&jj, &base, &orig, &evolog) {
            build_error = Some(e);
        } else {
            for extracted in &mut built {
                match jj.is_conflict(&extracted.change_id) {
                    Ok(conflict) => extracted.conflict = conflict,
                    Err(e) => {
                        build_error = Some(e);
                        break;
                    }
                }
            }
        }
    }
    if let Err(e) = jj
        .edit(&orig)
        .require("could not restore the original working copy")
    {
        err(&e);
        return 1;
    }
    if let Some(e) = build_error {
        err(&format!(
            "Extraction stopped: {e}\nThe original working copy was restored; rerun after fixing the reported jj error."
        ));
        return 1;
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
    match set_jj_alias() {
        Ok(()) => {
            println!("  alias:    `jj extract …` (jj user config → aliases.extract)");
        }
        Err(e) => {
            err(&format!(
                "Hooks were installed, but the `jj extract` alias could not be configured: {e}"
            ));
            return 1;
        }
    }
    println!(
        "\nRecording is automatic. Agents run `jj extract -m \"<task>\"` to pull their change."
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
    match unset_jj_alias() {
        Ok(()) => 0,
        Err(e) => {
            err(&format!(
                "Hooks were removed, but the `jj extract` alias could not be removed: {e}"
            ));
            1
        }
    }
}

/// Register `jj extract …` as a jj user alias that shells out to this binary via
/// `jj util exec` (the documented pattern for external jj subcommands).
fn set_jj_alias() -> Result<(), String> {
    let exe = current_exe();
    let alias = vec![
        "util".to_string(),
        "exec".to_string(),
        "--".to_string(),
        exe,
    ];
    if get_jj_alias()?.as_ref() == Some(&alias) {
        return Ok(());
    }
    let value = serde_json::to_string(&alias).unwrap();
    run_jj_config(std::process::Command::new("jj").args([
        "config",
        "set",
        "--user",
        "aliases.extract",
        &value,
    ]))
}

fn unset_jj_alias() -> Result<(), String> {
    let Some(alias) = get_jj_alias()? else {
        return Ok(());
    };
    let owned = alias
        .last()
        .and_then(|part| std::path::Path::new(part).file_name())
        .and_then(|name| name.to_str())
        == Some("jj-extract");
    if !owned {
        return Err(
            "refusing to remove aliases.extract because it is not a jj-extract alias".into(),
        );
    }
    run_jj_config(std::process::Command::new("jj").args([
        "config",
        "unset",
        "--user",
        "aliases.extract",
    ]))
}

fn get_jj_alias() -> Result<Option<Vec<String>>, String> {
    let output = std::process::Command::new("jj")
        .args(["config", "get", "aliases.extract"])
        .output()
        .map_err(|e| format!("could not run `jj`: {e}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        if detail.contains("No matching config key") || detail.contains("not found") {
            return Ok(None);
        }
        return Err(detail.trim().to_string());
    }
    serde_json::from_slice(&output.stdout)
        .map(Some)
        .map_err(|e| format!("could not parse the existing aliases.extract value: {e}"))
}

fn run_jj_config(command: &mut std::process::Command) -> Result<(), String> {
    let output = command
        .output()
        .map_err(|e| format!("could not run `jj`: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if detail.is_empty() {
        format!("`jj config` exited with {}", output.status)
    } else {
        detail
    })
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
            vec!["jj-extract", "--all", "--message", "ignored"],
            vec!["jj-extract", "--all", "--agent", "ignored"],
            vec!["jj-extract", "--install", "--all"],
            vec!["jj-extract", "--hook", "--all"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }
}

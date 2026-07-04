//! jj-collect — per-agent change attribution for a shared jj working copy.
//!
//! Multiple Claude agents edit one working directory; a hook routes each agent's
//! edits into that agent's own jj change (`base → agent1 → agent2 → @`), so every
//! agent's work is already isolated in its own commit — line-level, with jj's
//! diff/rebase engine doing all the content math. Installed as `jj-collect`, it
//! is also reachable as the native subcommand `jj collect …`.

mod hook;
mod identity;
mod install;
mod jj;
mod lock;
mod paths;
mod state;

use clap::{Parser, Subcommand};
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
    about = "Per-agent change attribution for a shared jj working copy.",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Hook entry point (used in settings.json; not for manual use).
    Hook,
    /// Register the collection hooks in Claude settings (run once).
    Install {
        /// Write ./.claude/settings.json (this repo) instead of the global file.
        #[arg(long)]
        project: bool,
        /// Explicit settings.json to patch.
        #[arg(long)]
        settings: Option<PathBuf>,
        /// Override the hook command.
        #[arg(long)]
        command: Option<String>,
    },
    /// Remove the collection hooks from Claude settings.
    Uninstall {
        #[arg(long)]
        project: bool,
        #[arg(long)]
        settings: Option<PathBuf>,
    },
    /// List agents and the change each is collecting in this repo.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show the change I (this session) am collecting.
    Mine {
        #[arg(long, help = "Agent id, index from 'list', or unique prefix.")]
        agent: Option<String>,
    },
    /// Harvest an agent's collected change onto the stack base as its own commit.
    Commit {
        #[arg(short = 'm', long)]
        message: Option<String>,
        #[arg(long, help = "Agent id, index from 'list', or unique prefix.")]
        agent: Option<String>,
        /// Just name the in-stack change; don't lift a standalone copy onto base.
        #[arg(long)]
        in_place: bool,
    },
    /// Forget collection state (does not touch your changes or commits).
    Reset {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Print the central data dir backing the current repo.
    Where,
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.cmd {
        Cmd::Hook => hook::run_hook(),
        Cmd::Install { project, settings, command } => cmd_install(project, settings, command),
        Cmd::Uninstall { project, settings } => cmd_uninstall(project, settings),
        Cmd::List { json } => cmd_list(json),
        Cmd::Mine { agent } => cmd_mine(agent),
        Cmd::Commit { message, agent, in_place } => cmd_commit(message, agent, in_place),
        Cmd::Reset { agent, all } => cmd_reset(agent, all),
        Cmd::Where => cmd_where(),
    };
    exit(code);
}

// --------------------------------------------------------------------------- //
// Shared helpers
// --------------------------------------------------------------------------- //
fn repo_or_exit() -> (PathBuf, PathBuf) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match find_repo_root(&cwd) {
        Some(root) => {
            let base = data_dir_for_root(&root);
            (root, base)
        }
        None => {
            err("Not inside a jj repo.");
            exit(2);
        }
    }
}

fn recency_order(state: &State) -> Vec<String> {
    let mut agents: Vec<(&String, &state::AgentRec)> = state.agents.iter().collect();
    agents.sort_by(|a, b| b.1.last_ts.cmp(&a.1.last_ts).then_with(|| a.0.cmp(b.0)));
    agents.into_iter().map(|(id, _)| id.clone()).collect()
}

/// Resolve a user-supplied --agent value: exact id, else 1-based `list` index,
/// else a unique name prefix/substring.
fn resolve_agent_ref(state: &State, r: &str) -> Option<String> {
    let order = recency_order(state);
    if order.iter().any(|a| a == r) {
        return Some(r.to_string());
    }
    if let Ok(n) = r.parse::<usize>() {
        if n >= 1 && n <= order.len() {
            return Some(order[n - 1].clone());
        }
    }
    let by_prefix: Vec<&String> = order.iter().filter(|a| a.starts_with(r)).collect();
    let matches = if by_prefix.is_empty() {
        order.iter().filter(|a| a.contains(r)).collect::<Vec<_>>()
    } else {
        by_prefix
    };
    if matches.len() == 1 {
        return Some(matches[0].clone());
    }
    if matches.is_empty() {
        err(&format!("No agent matches {r:?}."));
        eprint_menu(&order);
    } else {
        err(&format!("{r:?} is ambiguous. Use the index or a longer prefix."));
        eprint_menu(&order);
    }
    None
}

fn resolve_agent_query(state: &State, explicit: Option<&str>) -> Option<String> {
    match explicit {
        Some(r) => resolve_agent_ref(state, r),
        None => match from_cli(None) {
            Some(a) => Some(a),
            None => {
                err(&format!(
                    "Could not determine which agent you are. Pass --agent <id> or set ${ENV_VAR}."
                ));
                None
            }
        },
    }
}

fn eprint_menu(order: &[String]) {
    if order.is_empty() {
        eprintln!("  (no agents yet)");
    }
    for (i, a) in order.iter().enumerate() {
        eprintln!("  [{}] {a}", i + 1);
    }
}

// --------------------------------------------------------------------------- //
// Commands
// --------------------------------------------------------------------------- //
fn cmd_where() -> i32 {
    let (_root, base) = repo_or_exit();
    println!("{}", base.display());
    0
}

fn cmd_list(json: bool) -> i32 {
    let (root, base) = repo_or_exit();
    let state = State::load(&base);
    let jj = Jj::new(&root);
    let order = recency_order(&state);

    if json {
        let agents: Vec<serde_json::Value> = order
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let rec = &state.agents[a];
                serde_json::json!({
                    "index": i + 1,
                    "id": a,
                    "change_id": rec.change_id,
                    "resolves": jj.exists(&rec.change_id),
                    "conflict": jj.is_conflict(&rec.change_id),
                    "files": rec.files,
                    "last_active": rec.last_ts,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "base": state.base,
                "agents": agents,
            }))
            .unwrap()
        );
        return 0;
    }

    if order.is_empty() {
        println!("No collected changes yet.");
        return 0;
    }
    match &state.base {
        Some(b) => println!("{}  base {b}\n", dim("stack:")),
        None => println!("{}\n", dim("stack: (base not established yet)")),
    }
    for (i, a) in order.iter().enumerate() {
        let rec = &state.agents[a];
        let flags = if !jj.exists(&rec.change_id) {
            yellow(" (change gone)")
        } else if jj.is_conflict(&rec.change_id) {
            yellow(" (conflict)")
        } else {
            String::new()
        };
        println!(
            "{} {}{}",
            cyan(&format!("[{}] {a}", i + 1)),
            dim(&format!("→ {} · {} file(s) · {}", rec.change_id, rec.files.len(), ago(rec.last_ts))),
            flags
        );
        for f in &rec.files {
            println!("      {f}");
        }
        println!();
    }
    println!("Harvest with e.g. `jj collect commit --agent 1 -m \"...\"`.");
    0
}

fn cmd_mine(agent: Option<String>) -> i32 {
    let (root, base) = repo_or_exit();
    let state = State::load(&base);
    let who = match resolve_agent_query(&state, agent.as_deref()) {
        Some(w) => w,
        None => return 2,
    };
    let jj = Jj::new(&root);
    match state.agents.get(&who) {
        Some(rec) => {
            let flag = if jj.is_conflict(&rec.change_id) { yellow(" (conflict)") } else { String::new() };
            println!("{} {} {} file(s){}", bold(&format!("agent {who}")), dim(&format!("→ {}", rec.change_id)), rec.files.len(), flag);
            for f in &rec.files {
                println!("  {f}");
            }
            0
        }
        None => {
            println!("Nothing collected for agent {who}.");
            0
        }
    }
}

fn cmd_commit(message: Option<String>, agent: Option<String>, in_place: bool) -> i32 {
    let (root, base_dir) = repo_or_exit();
    let jj = Jj::new(&root);
    // Serialize against concurrent hooks: harvesting rewrites the stack.
    let _guard = RepoLock::acquire(&base_dir).ok();
    let state = State::load(&base_dir);

    let who = match resolve_agent_query(&state, agent.as_deref()) {
        Some(w) => w,
        None => return 2,
    };
    let rec = match state.agents.get(&who) {
        Some(r) if jj.exists(&r.change_id) => r.clone(),
        Some(_) => {
            err(&format!("agent {who}'s change no longer exists in the repo."));
            return 1;
        }
        None => {
            println!("Nothing collected for agent {who}.");
            return 0;
        }
    };

    if in_place {
        if let Some(m) = &message {
            let r = jj.describe(&rec.change_id, m);
            if !r.ok {
                err(&format!("describe failed: {}", r.stderr.trim()));
                return 1;
            }
        }
        let flag = if jj.is_conflict(&rec.change_id) { yellow(" (conflict)") } else { String::new() };
        println!(
            "{} agent {who} → change {} — {} file(s){}",
            green("✓"),
            rec.change_id,
            rec.files.len(),
            flag
        );
        return 0;
    }

    let base = match &state.base {
        Some(b) => b.clone(),
        None => {
            err("No stack base recorded yet.");
            return 1;
        }
    };
    // Lift a standalone copy of the agent's change onto base. jj re-applies it
    // via 3-way merge; a genuine overlap with another agent surfaces as a
    // conflict in the produced commit rather than silently merging.
    let dup = match jj.duplicate_onto(&rec.change_id, &base) {
        Some(d) => d,
        None => {
            err("failed to lift the change onto base.");
            return 1;
        }
    };
    if let Some(m) = &message {
        let r = jj.describe(&dup, m);
        if !r.ok {
            err(&format!("describe failed: {}", r.stderr.trim()));
        }
    }
    let commit = jj.commit_id(&dup).unwrap_or_else(|| dup.clone());
    if jj.is_conflict(&dup) {
        println!(
            "{} agent {who} → change {dup} ({commit}) — {} file(s)",
            yellow("⚠ harvested WITH CONFLICTS:"),
            rec.files.len()
        );
        println!("  another agent edited the same lines. Inspect: jj show {dup}");
    } else {
        println!(
            "{} agent {who} → change {dup} ({commit}) on base — {} file(s)",
            green("✓ harvested"),
            rec.files.len()
        );
        println!("  inspect: jj show {dup}");
    }
    0
}

fn cmd_reset(agent: Option<String>, all: bool) -> i32 {
    let (_root, base_dir) = repo_or_exit();
    if agent.is_none() && !all {
        err("Specify --agent <id> or --all.");
        return 2;
    }
    let _guard = RepoLock::acquire(&base_dir).ok();
    let mut state = State::load(&base_dir);
    if all {
        state = State::default();
    } else if let Some(a) = agent {
        match resolve_agent_ref(&state, &a) {
            Some(who) => {
                state.agents.remove(&who);
            }
            None => return 2,
        }
    }
    if let Err(e) = state.save(&base_dir) {
        err(&format!("save: {e}"));
        return 1;
    }
    println!("{} (jj changes are left untouched)", green("cleared"));
    0
}

fn cmd_install(project: bool, settings: Option<PathBuf>, command: Option<String>) -> i32 {
    let target = settings.unwrap_or_else(|| default_settings_path(project));
    let full = command.unwrap_or_else(default_hook_command);
    match install::merge_settings(&target, &full) {
        Ok(added) => {
            let verb = if added { "Installed" } else { "Already present —" };
            let scope = if project { "project" } else { "global" };
            println!("{} hooks ({scope}) → {}", green(verb), target.display());
            println!("  command:  {full}");
            println!("  records:  automatically in every jj repo (data in ~/.claude/jj-collect/)");
            if set_jj_alias() {
                println!("  alias:    `jj collect …` (jj user config → aliases.collect)");
            }
            0
        }
        Err(e) => {
            err(&format!("could not write {}: {e}", target.display()));
            1
        }
    }
}

fn cmd_uninstall(project: bool, settings: Option<PathBuf>) -> i32 {
    let target = settings.unwrap_or_else(|| default_settings_path(project));
    unset_jj_alias();
    match install::remove_settings(&target) {
        Ok(n) => {
            println!("{} removed {n} hook entr{} from {}", green("✓"), if n == 1 { "y" } else { "ies" }, target.display());
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
/// true on success; best-effort, so a missing jj is not fatal to `install`.
fn set_jj_alias() -> bool {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "jj-collect".to_string());
    // A TOML/JSON array; `--` makes jj pass trailing flags to us, not parse them.
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
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "jj-collect".to_string());
    format!("\"{exe}\" hook")
}

// --------------------------------------------------------------------------- //
// Tiny terminal styling (no external crates)
// --------------------------------------------------------------------------- //
fn use_color() -> bool {
    std::io::stdout().is_terminal()
}
fn paint(s: &str, code: &str) -> String {
    if use_color() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}
fn green(s: &str) -> String { paint(s, "32") }
fn yellow(s: &str) -> String { paint(s, "33") }
fn cyan(s: &str) -> String { paint(s, "36;1") }
fn bold(s: &str) -> String { paint(s, "1") }
fn dim(s: &str) -> String { paint(s, "2") }

fn err(msg: &str) {
    eprintln!("{}", paint_err(msg));
}
fn paint_err(s: &str) -> String {
    if std::io::stderr().is_terminal() {
        format!("\x1b[31m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn ago(ts: u64) -> String {
    if ts == 0 {
        return "?".into();
    }
    let now = state::now();
    let d = now.saturating_sub(ts);
    for (size, unit) in [(86400u64, "d"), (3600, "h"), (60, "m")] {
        if d >= size {
            return format!("{}{unit} ago", d / size);
        }
    }
    format!("{d}s ago")
}

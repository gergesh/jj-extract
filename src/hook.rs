//! Claude Code hook entry point (`jj-collect --hook`). Reads the hook JSON on
//! stdin and, for a session that has opted in via `jj collect`, squashes that
//! session's edit into its bound change.
//!
//! Contract: this must NEVER interfere with the tool call. It always exits 0,
//! emits nothing on stdout, and swallows every error (logged best-effort).
//!
//! Race-safety: every repo mutation runs while holding the repo's exclusive
//! [`RepoLock`]. Concurrent agent hooks therefore serialize, and because each
//! hook squashes only *its own* files out of `@`, a peer's simultaneous edit to
//! a different file stays in `@` untouched until that peer's own hook claims it.

use serde_json::Value;
use std::io::Read;
use std::path::Path;

use crate::identity::{from_payload, ENV_VAR};
use crate::jj::Jj;
use crate::paths::{central_root, ensure_data_dir, find_repo_root, relpath_within};
use crate::state::State;

const FILE_TOOLS: [&str; 3] = ["Edit", "Write", "MultiEdit"];

pub fn run_hook() -> i32 {
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        return 0;
    }
    let payload: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(&payload)));
    if result.is_err() {
        log_error("panic while dispatching hook");
    }
    0
}

fn dispatch(payload: &Value) {
    let event = payload.get("hook_event_name").and_then(Value::as_str).unwrap_or("");

    if event == "SessionStart" {
        session_start(payload);
        return;
    }
    if event != "PreToolUse" && event != "PostToolUse" {
        return;
    }
    let tool = payload.get("tool_name").and_then(Value::as_str).unwrap_or("");
    if !FILE_TOOLS.contains(&tool) {
        return;
    }

    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .unwrap_or_else(|| std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default());
    // Collection is jj-native: nothing to do outside a jj working copy.
    let root = match find_repo_root(Path::new(&cwd)) {
        Some(r) => r,
        None => return,
    };
    let agent = from_payload(payload.get("session_id").and_then(Value::as_str));

    let base_dir = match ensure_data_dir(&root) {
        Ok(b) => b,
        Err(e) => {
            log_error(&format!("ensure_data_dir: {e}"));
            return;
        }
    };
    // Opt-in (read-only check): only sessions that ran `jj collect` participate,
    // and only those take the edit lock, so a non-collecting session never blocks.
    if !State::load(&base_dir).bindings.contains_key(&agent) {
        return;
    }
    let jj = Jj::new(&root);
    let paths = edited_paths(payload, &|p| relpath_within(p, &root));

    if event == "PreToolUse" {
        // Take the edit lock and hold it *past this process* — PostToolUse
        // releases it. A blocked peer PreToolUse blocks its tool, so no other
        // edit can write until this one's whole bracket completes.
        crate::lock::acquire(&base_dir, &agent);
        if !paths.is_empty() {
            // Load state fresh *under* the lock: ensure_holding saves it, and a
            // read from before the lock could clobber a peer's just-added binding.
            let mut state = State::load(&base_dir);
            pre_tool(&jj, &mut state, &base_dir, &paths);
        }
    } else {
        if !paths.is_empty() {
            let state = State::load(&base_dir);
            post_tool(&jj, &state, &agent, &paths);
        }
        // Always release: this session held the lock since PreToolUse, even if
        // there was nothing to squash or the change vanished.
        crate::lock::release(&base_dir, &agent);
    }
}

/// Before the tool runs, park the target files' *current* `@` content into the
/// holding change, so `@` is clean for them. Whatever the tool then writes is
/// the only delta on those files at PostToolUse — nothing pre-existing (a Bash
/// command, the human, a peer) leaks into the agent's collected change.
fn pre_tool(jj: &Jj, state: &mut State, base_dir: &Path, paths: &[String]) {
    let holding = match crate::stack::ensure_holding(jj, state, base_dir) {
        Some(h) => h,
        None => return,
    };
    jj.file_track(paths);
    jj.snapshot();
    // Path-scoped: only the about-to-be-edited files move. A no-op (files
    // already clean) is fine and ignored.
    let _ = jj.squash_paths_into(&holding, paths);
}

/// After the tool runs, squash the (now isolated) delta on the target files into
/// the session's bound change.
fn post_tool(jj: &Jj, state: &State, agent: &str, paths: &[String]) {
    let change = match state.bindings.get(agent) {
        Some(c) => c.clone(),
        None => return,
    };
    if !jj.exists(&change) {
        // The bound change was abandoned; leave the edit in @ rather than guess.
        return;
    }
    jj.file_track(paths);
    jj.snapshot();
    let r = jj.squash_paths_into(&change, paths);
    if !r.ok {
        log_error(&format!("squash into {change} failed: {}", r.stderr.trim()));
    }
}

/// Extract repo-relative edited paths from a tool payload (Edit/Write/MultiEdit).
fn edited_paths(payload: &Value, to_rel: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let ti = match payload.get("tool_input") {
        Some(v) => v,
        None => return vec![],
    };
    let mut raw: Vec<String> = vec![];
    if let Some(fp) = ti.get("file_path").and_then(Value::as_str) {
        raw.push(fp.to_string());
    }
    if let Some(edits) = ti.get("edits").and_then(Value::as_array) {
        for e in edits {
            if let Some(fp) = e.get("file_path").and_then(Value::as_str) {
                raw.push(fp.to_string());
            }
        }
    }
    let mut out: Vec<String> = vec![];
    for p in raw {
        if let Some(rel) = to_rel(&p) {
            if !out.contains(&rel) {
                out.push(rel);
            }
        }
    }
    out
}

fn session_start(payload: &Value) {
    // Stamp JJ_COLLECT_AGENT=<session_id> into $CLAUDE_ENV_FILE so the agent's
    // own `jj collect` call knows its identity. Respect an already-set value.
    if std::env::var(ENV_VAR).map(|v| !v.is_empty()).unwrap_or(false) {
        return;
    }
    let env_file = match std::env::var("CLAUDE_ENV_FILE") {
        Ok(f) if !f.is_empty() => f,
        _ => return,
    };
    let sid = match payload.get("session_id").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s,
        _ => return,
    };
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&env_file) {
        let _ = writeln!(f, "{ENV_VAR}={sid}");
    }
}

fn log_error(message: &str) {
    let croot = central_root();
    let _ = std::fs::create_dir_all(&croot);
    if let Ok(mut f) =
        std::fs::OpenOptions::new().create(true).append(true).open(croot.join("hook-error.log"))
    {
        use std::io::Write;
        let _ = writeln!(f, "{message}");
    }
}

//! Claude Code hook entry point. Reads the hook JSON on stdin and routes each
//! agent's edit into that agent's collecting jj change.
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
use crate::lock::RepoLock;
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

    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .unwrap_or_else(|| std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default());
    // Recording is jj-native: nothing to do outside a jj working copy.
    let root = match find_repo_root(Path::new(&cwd)) {
        Some(r) => r,
        None => return,
    };
    let tool = payload.get("tool_name").and_then(Value::as_str).unwrap_or("");
    if !FILE_TOOLS.contains(&tool) {
        return;
    }

    let base_dir = match ensure_data_dir(&root) {
        Ok(b) => b,
        Err(e) => {
            log_error(&format!("ensure_data_dir: {e}"));
            return;
        }
    };
    let _guard = match RepoLock::acquire(&base_dir) {
        Ok(g) => g,
        Err(e) => {
            log_error(&format!("lock: {e}"));
            return;
        }
    };
    let jj = Jj::new(&root);

    match event {
        // Before the first edit lands, seal whatever the user already had into
        // the stack base so it is never attributed to an agent.
        "PreToolUse" => {
            let mut state = State::load(&base_dir);
            ensure_base(&jj, &mut state, &base_dir, true);
        }
        "PostToolUse" => {
            let mut state = State::load(&base_dir);
            route_edit(payload, &jj, &mut state, &base_dir);
        }
        _ => {}
    }
}

/// Establish the stack floor once. Agents insert their changes *beneath* `@`, so
/// the floor is `@`'s parent. With `seal` (the PreToolUse path), `@` holds the
/// user's pre-existing work — we push it into a fresh commit so it becomes the
/// floor and agents build on an empty `@`. Without `seal` (the rare PostToolUse
/// fallback when PreToolUse didn't run), `@` already holds the edit; we just
/// anchor the floor at `@-` and let routing claim the edit for its agent.
fn ensure_base(jj: &Jj, state: &mut State, base_dir: &Path, seal: bool) {
    if state.base.is_some() {
        return;
    }
    jj.snapshot();
    if seal && !jj.working_is_empty() {
        if !jj.new_empty_child().ok {
            return;
        }
    }
    let base = match jj.change_id("@-") {
        Some(b) => b,
        None => return,
    };
    state.base = Some(base);
    if let Err(e) = state.save(base_dir) {
        log_error(&format!("save base: {e}"));
    }
}

fn route_edit(payload: &Value, jj: &Jj, state: &mut State, base_dir: &Path) {
    // Fallback: if PreToolUse never ran, anchor the floor at @- without sealing
    // so this edit stays in @ and gets claimed for its agent below.
    if state.base.is_none() {
        ensure_base(jj, state, base_dir, false);
    }

    let session_id = payload.get("session_id").and_then(Value::as_str);
    let agent = from_payload(session_id);

    let root_rel = |p: &str| relpath_within(p, jj.root_path());
    let paths = edited_paths(payload, &root_rel);
    if paths.is_empty() {
        return;
    }

    // auto-track=none: make sure freshly-created files are visible to snapshots.
    jj.file_track(&paths);
    jj.snapshot();

    // Find (or create) this agent's collecting change. Recreate if a stored id
    // no longer resolves (e.g. the user abandoned it).
    let change = match state.agents.get(&agent) {
        Some(rec) if jj.exists(&rec.change_id) => rec.change_id.clone(),
        _ => match jj.insert_agent_change(&agent) {
            Some(c) => c,
            None => {
                log_error("insert_agent_change failed");
                return;
            }
        },
    };

    let r = jj.squash_paths_into(&change, &paths);
    if !r.ok {
        log_error(&format!("squash into {change} failed: {}", r.stderr.trim()));
        return;
    }

    state.touch(&agent, &change, &paths);
    if let Err(e) = state.save(base_dir) {
        log_error(&format!("save state: {e}"));
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
    // own CLI calls know their identity. Respect an already-set value.
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

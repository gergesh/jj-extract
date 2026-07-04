//! Claude Code hook entry point (`jj-collect --hook`). Reads the hook JSON on
//! stdin and, for a session that opted in via `jj collect`, **records** a cheap
//! snapshot pointer around each edit. It never mutates the collect stack and
//! takes no lock — the only shared thing it touches is jj's working-copy
//! snapshot (which jj serializes itself), so concurrent agents don't contend.
//!
//! Contract: never interfere with the tool call — always exit 0, emit nothing on
//! stdout, swallow every error (logged best-effort).

use serde_json::Value;
use std::io::Read;
use std::path::Path;

use crate::identity::{from_payload, ENV_VAR};
use crate::jj::Jj;
use crate::paths::{central_root, ensure_data_dir, find_repo_root, relpath_within};
use crate::store::{self, Event};

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
    // Opt-in: only a session that ran `jj collect` is recorded.
    if !store::session_active(&base_dir, &agent) {
        return;
    }
    let jj = Jj::new(&root);

    if event == "PreToolUse" {
        // Capture the pre-image: snapshot @ and stash its commit id for Post.
        if let Some(pre) = jj.snapshot_commit() {
            store::pending_set(&base_dir, &agent, &pre);
        }
        return;
    }

    // PostToolUse: track (so new files are visible), snapshot the post-image, and
    // append the (pre, post, files) tool-use record.
    let files = edited_paths(payload, &|p| relpath_within(p, &root));
    if files.is_empty() {
        return;
    }
    jj.file_track(&files);
    let post = match jj.snapshot_commit() {
        Some(p) => p,
        None => return,
    };
    // If Pre didn't run, fall back to the post itself as pre (records an empty
    // delta rather than mis-attributing).
    let pre = store::pending_take(&base_dir, &agent).unwrap_or_else(|| post.clone());
    store::event_append(&base_dir, &Event { session: agent, pre, post, files });
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

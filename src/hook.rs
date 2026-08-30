//! Claude Code and Codex hook entry point (`jj-extract --hook`). Records each
//! edit as an agent-tagged jj snapshot; jj's evolog is the attributed ledger.
//! Recording is automatic, so `jj extract` can pull any session's edits later.
//!
//! Claude's file tools and Codex's `apply_patch` are hooked, and so is a Bash
//! call that does nothing but write files (see [`crate::shell`]) — agents create
//! files with `cat > f <<'EOF'` and inline scripts as readily as with `Write`.
//! Every other tool's file effects are never collected: they're flushed to an
//! *unattributed* evolution by the neutral pre-snapshot, so they can't fold into
//! an agent's change.
//!
//! An edit never starts *tracking* a file: only the paths the tool creates are
//! offered to the snapshot as trackable (see [`crate::new_files`]), so a file the
//! user left untracked stays untracked however often an agent edits it.
//!
//! Each edit is bracketed by the **edit lock** (Pre takes it, Post releases it),
//! so a peer can't write while this edit is in flight — the agent's snapshot then
//! captures only its own edit. The lock guards just a fast write+snapshot, so
//! holding it across the tool is cheap, and it's uncontended when a session is
//! editing alone.
//!
//! Contract: never interfere with the tool call — always exit 0, emit nothing on
//! stdout, swallow every error (logged best-effort).

use serde_json::Value;
use std::io::Read;
use std::path::Path;

use crate::identity::{self, from_payload, ENV_VAR};
use crate::jj::Jj;
use crate::new_files;
use crate::paths::{central_root, find_repo_root, relpath_within};

const FILE_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "apply_patch"];
const SHELL_TOOL: &str = "Bash";

/// A tool call this hook records: which agent product made it, and the paths it
/// names as targets.
struct Recorded {
    kind: Option<&'static str>,
    targets: Vec<String>,
}

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
    let event = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    if event == "SessionStart" {
        session_start(payload);
        return;
    }
    if event != "PreToolUse" && event != "PostToolUse" {
        return;
    }
    let tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let recorded = match recorded_call(tool, payload) {
        Some(recorded) => recorded,
        None => return,
    };

    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        });
    let root = match find_repo_root(Path::new(&cwd)) {
        Some(r) => r,
        None => return,
    };
    let agent = from_payload(payload.get("session_id").and_then(Value::as_str));

    // The lock and the new-file note live in the repo's own `.jj/` — no central
    // data dir needed.
    let state_dir = root.join(".jj");
    let jj = Jj::new(&root);

    if event == "PreToolUse" {
        // Hold the edit lock across the write (released at PostToolUse) so no peer
        // writes meanwhile. The neutral snapshot flushes anything already on disk
        // (a Bash/human change) to an unattributed evolution.
        crate::lock::acquire(&state_dir, &agent);
        // Under that lock, note which targets don't exist yet: those, and only
        // those, are the files this edit may start tracking.
        let creating: Vec<String> = recorded
            .targets
            .iter()
            .filter_map(|target| relpath_within(target, &root))
            .filter(|file| !root.join(file).exists())
            .collect();
        new_files::record(&state_dir, &agent, &creating);
        jj.snapshot_neutral();
        return;
    }

    // PostToolUse: capture the edit as this agent's evolution, then release. The
    // snapshot always covers every tracked file; the paths only say what may
    // *become* tracked, so an edit to an untracked file leaves it untracked.
    let created = new_files::take(&state_dir, &agent);
    jj.snapshot_tagged(&agent, recorded.kind, &created);
    crate::lock::release(&state_dir, &agent);
}

/// What this tool call means for recording, or None when its file effects stay
/// unattributed.
fn recorded_call(tool: &str, payload: &Value) -> Option<Recorded> {
    if FILE_TOOLS.contains(&tool) {
        return Some(Recorded {
            kind: identity::kind_of_tool(tool),
            targets: edited_paths(payload),
        });
    }
    if tool != SHELL_TOOL {
        return None;
    }
    let input = payload.get("tool_input")?;
    // A backgrounded command is still running when PostToolUse fires, so its
    // writes would land in whatever evolution happens to come next.
    if input
        .get("run_in_background")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let command = input.get("command").and_then(Value::as_str)?;
    Some(Recorded {
        kind: Some(identity::CLAUDE),
        targets: crate::shell::written_paths(command)?,
    })
}

/// Extract the paths named by Claude file-tool or Codex apply_patch payloads.
fn edited_paths(payload: &Value) -> Vec<String> {
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
    // Codex sends apply_patch's complete patch text in `tool_input.command`.
    // Capture old and new paths so add/delete/move operations all snapshot with
    // the session's identity.
    if let Some(command) = ti.get("command").and_then(Value::as_str) {
        for line in command.lines() {
            for prefix in [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ] {
                if let Some(path) = line.strip_prefix(prefix) {
                    let path = path.trim();
                    if !path.is_empty() {
                        raw.push(path.to_string());
                    }
                    break;
                }
            }
        }
    }
    let mut out: Vec<String> = vec![];
    for path in raw {
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

fn session_start(payload: &Value) {
    // Stamp JJ_EXTRACT_AGENT=<session_id> into $CLAUDE_ENV_FILE so the agent's own
    // `jj extract` call knows its identity. Respect an already-set value.
    //
    // $CLAUDE_ENV_FILE is *sourced* as a shell prefix, so the line MUST say
    // `export` — a bare `KEY=VALUE` sets an unexported shell var that never reaches
    // the `jj-extract` process `jj extract` starts via `jj util exec`. (Even so,
    // `from_cli` falls back to the always-exported $CLAUDE_CODE_SESSION_ID.)
    if std::env::var(ENV_VAR)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
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
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&env_file)
    {
        // Single-quote the value (session ids are UUIDs — no quotes to escape).
        let _ = writeln!(f, "export {ENV_VAR}='{sid}'");
    }
}

fn log_error(message: &str) {
    let croot = central_root();
    let _ = std::fs::create_dir_all(&croot);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(croot.join("hook-error.log"))
    {
        use std::io::Write;
        let _ = writeln!(f, "{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::edited_paths;
    use serde_json::json;

    #[test]
    fn extracts_all_codex_apply_patch_paths_without_duplicates() {
        let payload = json!({
            "tool_input": {
                "command": "*** Begin Patch\n*** Update File: src/main.rs\n*** Move to: src/cli.rs\n*** Add File: docs/usage.md\n*** Delete File: old.txt\n*** Update File: src/main.rs\n*** End Patch"
            }
        });

        assert_eq!(
            edited_paths(&payload),
            ["src/main.rs", "src/cli.rs", "docs/usage.md", "old.txt"]
        );
    }
}

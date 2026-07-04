//! On-disk record store for one repo, under its central data dir. Deliberately
//! file-per-thing and append-only, so the hot path (the hooks) needs **no lock**:
//! nothing here does a read-modify-write of shared state.
//!
//! ```text
//!   base            the repo construction floor (write-once)
//!   session-<id>    marker that session <id> is collecting; contents = message
//!   pending-<id>    the pre-image commit from PreToolUse, consumed at PostToolUse
//!   events.jsonl    append-only log of {session, pre, post, files} per tool call
//! ```
//!
//! Session ids are Claude `session_id`s (UUID-ish, filesystem-safe), so they
//! round-trip through the `session-<id>` / `pending-<id>` filenames unchanged.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub session: String,
    /// `@` commit id captured at PreToolUse (before the edit).
    pub pre: String,
    /// `@` commit id captured at PostToolUse (after the edit).
    pub post: String,
    /// Repo-relative paths the edit touched — the delta is scoped to these.
    pub files: Vec<String>,
}

fn safe(id: &str) -> String {
    id.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect()
}

// --- base (write-once) ------------------------------------------------------
pub fn base_get(base_dir: &Path) -> Option<String> {
    std::fs::read_to_string(base_dir.join("base")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// First writer wins (`O_EXCL`); later calls are no-ops.
pub fn base_set_once(base_dir: &Path, commit: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create_new(true).write(true).open(base_dir.join("base")) {
        let _ = writeln!(f, "{commit}");
    }
}

// --- session markers --------------------------------------------------------
pub fn session_start(base_dir: &Path, session: &str, message: Option<&str>) {
    let _ = std::fs::write(base_dir.join(format!("session-{}", safe(session))), message.unwrap_or(""));
}

pub fn session_active(base_dir: &Path, session: &str) -> bool {
    base_dir.join(format!("session-{}", safe(session))).exists()
}

pub fn session_message(base_dir: &Path, session: &str) -> Option<String> {
    std::fs::read_to_string(base_dir.join(format!("session-{}", safe(session)))).ok().filter(|s| !s.is_empty())
}

/// Every session that has opted in (has a `session-*` marker).
pub fn sessions(base_dir: &Path) -> Vec<String> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(base_dir) {
        for e in rd.flatten() {
            if let Some(name) = e.file_name().to_str() {
                if let Some(id) = name.strip_prefix("session-") {
                    out.push(id.to_string());
                }
            }
        }
    }
    out.sort();
    out
}

// --- pending pre-image ------------------------------------------------------
pub fn pending_set(base_dir: &Path, session: &str, pre: &str) {
    let _ = std::fs::write(base_dir.join(format!("pending-{}", safe(session))), pre);
}

pub fn pending_take(base_dir: &Path, session: &str) -> Option<String> {
    let p = base_dir.join(format!("pending-{}", safe(session)));
    let v = std::fs::read_to_string(&p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let _ = std::fs::remove_file(&p);
    v
}

// --- event log --------------------------------------------------------------
/// Append one event. The whole line (JSON + `\n`) is written in a single
/// `write_all` under `O_APPEND` — one small write syscall, appended atomically —
/// so concurrent sessions can append without a lock. (A `writeln!` would split
/// into multiple syscalls and let concurrent appends interleave on one line.)
pub fn event_append(base_dir: &Path, e: &Event) {
    if let Ok(mut line) = serde_json::to_string(e) {
        line.push('\n');
        if let Ok(mut f) =
            std::fs::OpenOptions::new().create(true).append(true).open(base_dir.join("events.jsonl"))
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

pub fn events(base_dir: &Path) -> Vec<Event> {
    match std::fs::read_to_string(base_dir.join("events.jsonl")) {
        Ok(text) => text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect(),
        Err(_) => vec![],
    }
}

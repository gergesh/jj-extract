//! The new-file note: the paths a file tool is about to *create*, written by
//! PreToolUse and read back by PostToolUse.
//!
//! Snapshotting is reversible; tracking isn't. Once a path is tracked, jj keeps
//! snapshotting it, so a file the user deliberately left untracked (an ignored
//! artifact, a scratch file, a repo whose `snapshot.auto-track` is `none()`)
//! must stay untracked no matter how often an agent edits it. Only a file the
//! agent itself creates may start being tracked, and "created" is decided the
//! only way it can be decided reliably: the path did not exist on disk when the
//! tool was about to run.
//!
//! PreToolUse and PostToolUse are separate processes, so that observation is
//! carried across in a file beside the edit lock in `.jj/`. The same lock
//! brackets it, so only one edit's note exists at a time. The note names the
//! agent that wrote it and is read back only by that agent, so a note left
//! behind by a tool that never reached PostToolUse (denied, crashed) cannot be
//! mistaken for the next edit's — and each PreToolUse replaces it wholesale.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

fn note_path(base: &Path) -> PathBuf {
    base.join("jj-extract.new-files")
}

/// Record the paths `agent`'s in-flight edit is about to create, replacing any
/// earlier note.
pub fn record(base: &Path, agent: &str, paths: &[String]) {
    let _ = fs::create_dir_all(base);
    let note = json!({ "agent": agent, "paths": paths });
    let _ = fs::write(note_path(base), note.to_string());
}

/// Read back and clear `agent`'s note: the paths its edit may start tracking.
/// Anything unreadable, malformed, or another agent's yields no paths, so the
/// snapshot falls back to tracking nothing.
pub fn take(base: &Path, agent: &str) -> Vec<String> {
    let path = note_path(base);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => return vec![],
    };
    let _ = fs::remove_file(&path);
    let note: Value = match serde_json::from_str(&raw) {
        Ok(note) => note,
        Err(_) => return vec![],
    };
    if note.get("agent").and_then(Value::as_str) != Some(agent) {
        return vec![];
    }
    note.get("paths")
        .and_then(Value::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{record, take};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("jj-extract-note-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn round_trips_the_writing_agents_paths_once() {
        let base = scratch("roundtrip");
        record(&base, "a1", &["src/new.rs".to_string()]);

        assert_eq!(take(&base, "a1"), ["src/new.rs"]);
        assert!(take(&base, "a1").is_empty(), "the note is consumed");
    }

    #[test]
    fn ignores_a_note_left_behind_by_another_agent() {
        let base = scratch("foreign");
        record(&base, "a1", &["src/new.rs".to_string()]);

        assert!(take(&base, "a2").is_empty());
    }
}

//! Thin, well-defined wrappers over the `jj` CLI. Every sequence here was
//! validated against jj 0.42 before being encoded (see the design prototypes).
//!
//! Invariants relied on elsewhere:
//! * `jj` change ids are stable across the rebases/squashes we perform.
//! * We never pass `--ignore-working-copy` on a `@`-rewriting command: doing so
//!   desyncs the on-disk working copy ("stale working copy"). We let jj manage
//!   the working copy and serialize with an external lock instead.
//! * `snapshot.auto-track=none` is common in the wild, so new files are invisible
//!   until `jj file track`ed — we track an agent's paths before squashing them.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Jj {
    root: PathBuf,
}

pub struct Run {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Jj {
    pub fn new(root: &Path) -> Jj {
        Jj { root: root.to_path_buf() }
    }

    pub fn root_path(&self) -> &Path {
        &self.root
    }

    /// Run `jj <args>` in the repo, never opening an editor or a pager.
    pub fn run(&self, args: &[&str]) -> Run {
        let out = Command::new("jj")
            .args(args)
            .current_dir(&self.root)
            .env("JJ_EDITOR", "true") // never block on an interactive editor
            .env("JJ_CONFIG", std::env::var_os("JJ_CONFIG").unwrap_or_default())
            .output();
        match out {
            Ok(o) => Run {
                ok: o.status.success(),
                stdout: String::from_utf8_lossy(&o.stdout).to_string(),
                stderr: String::from_utf8_lossy(&o.stderr).to_string(),
            },
            Err(e) => Run { ok: false, stdout: String::new(), stderr: e.to_string() },
        }
    }

    /// Snapshot the on-disk working copy into `@` (what `jj status` does as a
    /// side effect). Cheap; used to materialize an edit before routing it.
    pub fn snapshot(&self) -> Run {
        self.run(&["status"])
    }

    /// Track the given (existing) paths so `auto-track=none` repos still see
    /// newly-created files. Missing paths (e.g. a deletion) are skipped.
    pub fn file_track(&self, paths: &[String]) {
        let existing: Vec<&str> = paths
            .iter()
            .filter(|p| self.root.join(p).exists())
            .map(|s| s.as_str())
            .collect();
        if existing.is_empty() {
            return;
        }
        let mut args = vec!["file", "track", "--"];
        args.extend(existing);
        let _ = self.run(&args);
    }

    /// The change id at `rev` (short form), or None if it doesn't resolve.
    pub fn change_id(&self, rev: &str) -> Option<String> {
        let r = self.run(&["log", "-r", rev, "-T", "change_id.short()", "--no-graph"]);
        let id = r.stdout.trim().to_string();
        if r.ok && !id.is_empty() {
            Some(id)
        } else {
            None
        }
    }

    /// The short commit id at `rev`, or None.
    pub fn commit_id(&self, rev: &str) -> Option<String> {
        let r = self.run(&["log", "-r", rev, "-T", "commit_id.short()", "--no-graph"]);
        let id = r.stdout.trim().to_string();
        (r.ok && !id.is_empty()).then_some(id)
    }

    pub fn exists(&self, rev: &str) -> bool {
        self.change_id(rev).is_some()
    }

    /// Is `@` empty (no changes relative to its parent)?
    pub fn working_is_empty(&self) -> bool {
        let r = self.run(&["log", "-r", "@", "-T", r#"if(empty,"1","0")"#, "--no-graph"]);
        r.stdout.trim() == "1"
    }

    pub fn is_conflict(&self, rev: &str) -> bool {
        let r = self.run(&["log", "-r", rev, "-T", r#"if(conflict,"1","0")"#, "--no-graph"]);
        r.stdout.trim() == "1"
    }

    /// Create a fresh empty child of `@` and check it out (used to seal the
    /// stack base, leaving the user's pre-existing work in the parent).
    pub fn new_empty_child(&self) -> Run {
        self.run(&["new"])
    }

    /// Insert an empty change tagged for `agent` directly beneath `@` *without*
    /// moving the working copy, and return its stable change id. The freshly
    /// inserted change becomes `@`'s parent (`@-`).
    pub fn insert_agent_change(&self, agent: &str) -> Option<String> {
        let msg = agent_message(agent);
        let r = self.run(&["new", "--no-edit", "--insert-before", "@", "-m", &msg]);
        if !r.ok {
            return None;
        }
        self.change_id("@-")
    }

    /// Move only `paths`' portion of `@`'s diff down into `change`, keeping
    /// `change`'s own description. Any other files' changes stay in `@` — the
    /// mechanism that lets a concurrent agent's edit remain unclaimed.
    pub fn squash_paths_into(&self, change: &str, paths: &[String]) -> Run {
        let mut args = vec![
            "squash",
            "--from",
            "@",
            "--into",
            change,
            "--use-destination-message",
        ];
        // Trailing fileset args select which files move.
        let owned: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
        args.extend(owned);
        self.run(&args)
    }

    pub fn describe(&self, change: &str, message: &str) -> Run {
        self.run(&["describe", "-r", change, "-m", message])
    }

    /// Duplicate `change` onto `dest`, re-applying it there via jj's 3-way
    /// merge. Returns the new change id. If the change genuinely overlaps what
    /// sits between it and `dest`, the duplicate is created *with conflicts*
    /// (still returned) — caller should check [`is_conflict`].
    pub fn duplicate_onto(&self, change: &str, dest: &str) -> Option<String> {
        let r = self.run(&["duplicate", change, "-d", dest]);
        if !r.ok {
            return None;
        }
        // "Duplicated <src> as <change_id> <commit_id> …" on stdout or stderr.
        let text = format!("{}\n{}", r.stdout, r.stderr);
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("Duplicated ") {
                let mut it = rest.split_whitespace();
                // <src> as <change_id> <commit_id>
                let _src = it.next();
                if it.next() == Some("as") {
                    if let Some(id) = it.next() {
                        return Some(id.to_string());
                    }
                }
            }
        }
        None
    }

}

/// The auto-generated description for an agent's live collecting change. Carries
/// a machine-readable trailer so the change is recoverable even without state.
pub fn agent_message(agent: &str) -> String {
    format!("jj-collect: {agent}\n\nCollect-Agent: {agent}\n")
}

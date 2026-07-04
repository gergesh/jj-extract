//! Thin, well-defined wrappers over the `jj` CLI. Every sequence here was
//! validated against jj 0.42 before being encoded.
//!
//! Invariants relied on elsewhere:
//! * `jj` change ids are stable across the rebases/squashes we perform.
//! * We never pass `--ignore-working-copy` on a `@`-rewriting command: doing so
//!   desyncs the on-disk working copy ("stale working copy"). We let jj manage
//!   the working copy and serialize with an external lock instead.
//! * We never *move* the shared `@`. All agents share one working copy; a
//!   collecting change is inserted *beneath* `@`, which stays a neutral scratch
//!   so a peer's not-yet-squashed edit is never absorbed into the wrong change.
//! * `snapshot.auto-track=none` is common, so new files are invisible until
//!   `jj file track`ed — we track an agent's paths before squashing them.

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

    /// Run `jj <args>` in the repo, never opening an editor or a pager.
    pub fn run(&self, args: &[&str]) -> Run {
        let out = Command::new("jj")
            .args(args)
            .current_dir(&self.root)
            .env("JJ_EDITOR", "true") // never block on an interactive editor
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
    /// side effect). Used to materialize an edit before routing it.
    pub fn snapshot(&self) -> Run {
        self.run(&["status"])
    }

    /// Track the given (existing) paths so `auto-track=none` repos still see
    /// newly-created files. Missing paths (e.g. a deletion) are skipped.
    pub fn file_track(&self, paths: &[String]) {
        let existing: Vec<&str> =
            paths.iter().filter(|p| self.root.join(p).exists()).map(|s| s.as_str()).collect();
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
        (r.ok && !id.is_empty()).then_some(id)
    }

    pub fn exists(&self, rev: &str) -> bool {
        self.change_id(rev).is_some()
    }

    /// Is `@` empty (no diff from its parent)?
    pub fn working_is_empty(&self) -> bool {
        let r = self.run(&["log", "-r", "@", "-T", r#"if(empty,"1","0")"#, "--no-graph"]);
        r.stdout.trim() == "1"
    }

    /// Push a fresh empty child of `@` and check it out — used once to seal the
    /// user's pre-existing work into the stack base.
    pub fn new_empty_child(&self) -> Run {
        self.run(&["new"])
    }

    /// Insert an empty change as a direct child of `rev` (rebasing rev's existing
    /// descendants on top), without moving `@`. Returns its change id. Used to
    /// place the holding change just above the base, beneath the agent changes.
    pub fn insert_after(&self, rev: &str, message: Option<&str>) -> Option<String> {
        let mut args = vec!["new", "--no-edit", "--insert-after", rev];
        if let Some(m) = message {
            args.push("-m");
            args.push(m);
        }
        let r = self.run(&args);
        if !r.ok {
            return None;
        }
        parse_created(&r.stdout, &r.stderr)
    }

    /// Mint an empty change directly beneath `@` *without* moving the working
    /// copy, and return its stable change id. The new change becomes `@`'s
    /// parent (`@-`). This is the `jj new`-like operation, minus relocating the
    /// shared `@`.
    pub fn mint_change(&self, message: Option<&str>) -> Option<String> {
        let mut args = vec!["new", "--no-edit", "--insert-before", "@"];
        if let Some(m) = message {
            args.push("-m");
            args.push(m);
        }
        let r = self.run(&args);
        if !r.ok {
            return None;
        }
        // The freshly inserted change is @'s parent; parse output as a fallback.
        self.change_id("@-").or_else(|| parse_created(&r.stdout, &r.stderr))
    }

    /// Move only `paths`' portion of `@`'s diff down into `change`, keeping
    /// `change`'s own description. Other files' changes stay in `@` — the
    /// mechanism that lets a concurrent agent's edit remain unclaimed.
    pub fn squash_paths_into(&self, change: &str, paths: &[String]) -> Run {
        let mut args =
            vec!["squash", "--from", "@", "--into", change, "--use-destination-message"];
        let owned: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
        args.extend(owned);
        self.run(&args)
    }

    pub fn describe(&self, change: &str, message: &str) -> Run {
        self.run(&["describe", "-r", change, "-m", message])
    }
}

/// Pull the change id out of a `jj new` line: "Created new commit <cid> <commit> …".
fn parse_created(stdout: &str, stderr: &str) -> Option<String> {
    for line in format!("{stdout}\n{stderr}").lines() {
        if let Some(rest) = line.trim().strip_prefix("Created new commit ") {
            if let Some(cid) = rest.split_whitespace().next() {
                return Some(cid.to_string());
            }
        }
    }
    None
}

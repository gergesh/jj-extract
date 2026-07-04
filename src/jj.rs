//! Thin, well-defined wrappers over the `jj` CLI. Two roles:
//!
//! * **Record** (hooks): `snapshot_commit` runs `jj log -r @` which snapshots the
//!   working copy as a side effect and returns `@`'s commit id — one cheap call
//!   that captures the current on-disk state as a content-addressed pointer.
//! * **Construct** (harvest): replay recorded (pre, post) snapshots into an
//!   isolated per-agent change — `new_on` a recorded pre, overwrite the edited
//!   files with their post content, snapshot to get a delta commit, then
//!   `rebase`/`squash` it so jj's 3-way merge composes each agent's edits.
//!
//! Invariant: recorded snapshot commit ids stay diff-/rebase-able after `@` moves
//! on (jj's store keeps them), which is what makes deferred construction work.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Jj {
    root: PathBuf,
}

pub struct Run {
    pub ok: bool,
    pub stdout: String,
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
            .env("JJ_EDITOR", "true")
            .output();
        match out {
            Ok(o) => Run {
                ok: o.status.success(),
                stdout: String::from_utf8_lossy(&o.stdout).to_string(),
            },
            Err(_) => Run { ok: false, stdout: String::new() },
        }
    }

    // --- record --------------------------------------------------------------

    /// Snapshot the working copy into `@` and return `@`'s commit id. `jj log`
    /// snapshots as a side effect, so this both captures the current on-disk
    /// state and yields the content-addressed pointer to it — in one call.
    pub fn snapshot_commit(&self) -> Option<String> {
        // Track first so `auto-track=none` doesn't hide newly-created files from
        // the snapshot. (Untracked-but-existing files are made visible.)
        let r = self.run(&["log", "-r", "@", "-T", "commit_id.short()", "--no-graph"]);
        let id = r.stdout.trim().to_string();
        (r.ok && !id.is_empty()).then_some(id)
    }

    /// Track the given (existing) paths, so `auto-track=none` repos still see
    /// newly-created files in the next snapshot. Missing paths are skipped.
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

    // --- construct -----------------------------------------------------------

    /// Check out `rev`'s content into a fresh working copy (`jj new <rev>`).
    pub fn new_on(&self, rev: &str) -> Run {
        self.run(&["new", rev])
    }

    /// Create a fresh empty child of `@` (moves `@` off whatever it was).
    pub fn new_empty(&self) -> Run {
        self.run(&["new"])
    }

    /// Snapshot the working copy (no output needed).
    pub fn snapshot(&self) {
        let _ = self.run(&["status"]);
    }

    /// The content of `path` at `rev`, or None if it doesn't exist there.
    pub fn file_show(&self, rev: &str, path: &str) -> Option<String> {
        let r = self.run(&["file", "show", "-r", rev, path]);
        r.ok.then_some(r.stdout)
    }

    /// Rebase `change` (and only it) onto `dest`, letting jj's 3-way merge apply
    /// its diff there. Conflicts (genuine same-region overlap) are recorded in
    /// the result rather than merged away.
    pub fn rebase_onto(&self, change: &str, dest: &str) -> Run {
        self.run(&["rebase", "-r", change, "-d", dest])
    }

    /// Squash `from`'s changes down into `into`, keeping `into`'s description.
    pub fn squash_into(&self, from: &str, into: &str) -> Run {
        self.run(&["squash", "--from", from, "--into", into, "--use-destination-message"])
    }

    // --- queries -------------------------------------------------------------

    pub fn change_id(&self, rev: &str) -> Option<String> {
        let r = self.run(&["log", "-r", rev, "-T", "change_id.short()", "--no-graph"]);
        let id = r.stdout.trim().to_string();
        (r.ok && !id.is_empty()).then_some(id)
    }

    pub fn is_conflict(&self, rev: &str) -> bool {
        let r = self.run(&["log", "-r", rev, "-T", r#"if(conflict,"1","0")"#, "--no-graph"]);
        r.stdout.trim() == "1"
    }

    pub fn describe(&self, rev: &str, message: &str) -> Run {
        self.run(&["describe", "-r", rev, "-m", message])
    }
}

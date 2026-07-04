//! Thin wrappers over the `jj` CLI. Two roles:
//!
//! * **Record** (hooks): each edit is captured as a working-copy snapshot whose
//!   *operation* is tagged with the acting agent (`JJ_OP_USERNAME`). jj's evolog
//!   then carries the attribution itself — `jj evolog -r @` lists every
//!   evolution of `@` with the operation (and thus agent) that made it. No
//!   sidecar: the op log is the ledger.
//! * **Construct** (harvest): read the evolog, and for each of an agent's
//!   evolutions replay `diff(previous, this)` as a delta commit rebased onto
//!   base, letting jj's 3-way merge compose the agent's edits.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Jj {
    root: PathBuf,
}

pub struct Run {
    pub ok: bool,
    pub stdout: String,
}

/// One evolution of `@`: its commit id and the username on the operation that
/// created it (i.e. the agent we tagged, or a neutral default).
pub struct Evolution {
    pub commit: String,
    pub user: String,
}

impl Jj {
    pub fn new(root: &Path) -> Jj {
        Jj { root: root.to_path_buf() }
    }

    fn run_env(&self, args: &[&str], env: &[(&str, &str)]) -> Run {
        let mut cmd = Command::new("jj");
        cmd.args(args).current_dir(&self.root).env("JJ_EDITOR", "true");
        for (k, v) in env {
            cmd.env(k, v);
        }
        match cmd.output() {
            Ok(o) => Run {
                ok: o.status.success(),
                stdout: String::from_utf8_lossy(&o.stdout).to_string(),
            },
            Err(_) => Run { ok: false, stdout: String::new() },
        }
    }

    pub fn run(&self, args: &[&str]) -> Run {
        self.run_env(args, &[])
    }

    // --- record --------------------------------------------------------------

    /// Snapshot the working copy under a *neutral* operation (default user), so
    /// whatever is on disk right now — a Bash/human/peer change — becomes an
    /// unattributed evolution and won't fold into the next agent's edit.
    pub fn snapshot_neutral(&self) {
        let _ = self.run(&["status"]);
    }

    /// Snapshot the working copy under an operation tagged with `agent`, so this
    /// becomes the agent's evolution in the evolog.
    ///
    /// A plain `jj status` tags the snapshot op correctly but won't pick up a
    /// newly-created file (`auto-track=none`); `jj file track` picks it up but
    /// does the content snapshot in a *separate untagged* op. So we snapshot with
    /// `snapshot.auto-track` scoped to the edited file — that both tracks it and
    /// tags the snapshot. One snapshot per file (edits are ~always single-file);
    /// each is a tagged evolution, and the builder composes them.
    pub fn snapshot_tagged(&self, agent: &str, paths: &[String]) {
        let env = [("JJ_OP_USERNAME", agent)];
        let existing: Vec<&str> =
            paths.iter().filter(|p| self.root.join(p).exists()).map(|s| s.as_str()).collect();
        if existing.is_empty() {
            let _ = self.run_env(&["status"], &env);
            return;
        }
        for p in existing {
            // `{:?}` quotes the path; jj reads the --config value as the fileset
            // to auto-track for this snapshot (unrelated untracked files stay out).
            let cfg = format!("snapshot.auto-track={p:?}");
            let _ = self.run_env(&["--config", &cfg, "status"], &env);
        }
    }

    // --- construct -----------------------------------------------------------

    /// `@`'s evolutions, oldest first, each with the tagging operation's user.
    pub fn evolog(&self) -> Vec<Evolution> {
        let r = self.run(&[
            "evolog",
            "-r",
            "@",
            "-T",
            r#"commit.commit_id().short() ++ " " ++ operation.user() ++ "\n""#,
            "--no-graph",
        ]);
        if !r.ok {
            return vec![];
        }
        let mut evos: Vec<Evolution> = r
            .stdout
            .lines()
            .filter_map(|l| {
                let (commit, user) = l.trim().split_once(' ')?;
                // op.user() is "name@host"; keep the name.
                let user = user.split('@').next().unwrap_or(user);
                Some(Evolution { commit: commit.to_string(), user: user.to_string() })
            })
            .collect();
        evos.reverse(); // evolog is newest-first; we want chronological
        evos
    }

    pub fn new_on(&self, rev: &str) -> Run {
        self.run(&["new", rev])
    }

    /// Make the working copy's content equal to `rev`'s (all files).
    pub fn restore_from(&self, rev: &str) -> Run {
        self.run(&["restore", "--from", rev])
    }

    pub fn new_empty(&self) -> Run {
        self.run(&["new"])
    }

    pub fn snapshot(&self) {
        let _ = self.run(&["status"]);
    }

    pub fn rebase_onto(&self, change: &str, dest: &str) -> Run {
        self.run(&["rebase", "-r", change, "-d", dest])
    }

    pub fn squash_into(&self, from: &str, into: &str) -> Run {
        self.run(&["squash", "--from", from, "--into", into, "--use-destination-message"])
    }

    /// Restore the working copy to `rev` in place, preserving its change id (and
    /// thus its evolog) — used to return `@` to the live state after harvest.
    pub fn edit(&self, rev: &str) -> Run {
        self.run(&["edit", rev])
    }

    /// Abandon `rev` (used to clean up the builder's throwaway pre-image commits).
    pub fn abandon(&self, rev: &str) -> Run {
        self.run(&["abandon", "-r", rev])
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

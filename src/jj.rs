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
    pub stderr: String,
}

impl Run {
    /// Turn a failed jj invocation into an actionable error while preserving the
    /// successful result for callers that need its output.
    pub fn require(self, context: &str) -> Result<Run, String> {
        if self.ok {
            return Ok(self);
        }
        let detail = self.stderr.trim();
        let detail = if detail.is_empty() {
            self.stdout.trim()
        } else {
            detail
        };
        if detail.is_empty() {
            Err(format!("{context}: jj exited unsuccessfully"))
        } else {
            Err(format!("{context}: {detail}"))
        }
    }
}

/// One evolution of `@`: its commit id and the username on the operation that
/// created it (i.e. the agent we tagged, or a neutral default).
pub struct Evolution {
    pub commit: String,
    pub user: String,
}

impl Jj {
    pub fn new(root: &Path) -> Jj {
        Jj {
            root: root.to_path_buf(),
        }
    }

    fn run_env(&self, args: &[&str], env: &[(&str, &str)]) -> Run {
        let mut cmd = Command::new("jj");
        cmd.args(args)
            .current_dir(&self.root)
            .env("JJ_EDITOR", "true");
        for (k, v) in env {
            cmd.env(k, v);
        }
        match cmd.output() {
            Ok(o) => Run {
                ok: o.status.success(),
                stdout: String::from_utf8_lossy(&o.stdout).to_string(),
                stderr: String::from_utf8_lossy(&o.stderr).to_string(),
            },
            Err(e) => Run {
                ok: false,
                stdout: String::new(),
                stderr: e.to_string(),
            },
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
        let existing: Vec<&str> = paths
            .iter()
            .filter(|p| self.root.join(p).exists())
            .map(|s| s.as_str())
            .collect();
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
    pub fn evolog(&self) -> Result<Vec<Evolution>, String> {
        let r = self
            .run(&[
                "evolog",
                "-r",
                "@",
                "-T",
                r#"commit.commit_id().short() ++ " " ++ operation.user() ++ "\n""#,
                "--no-graph",
            ])
            .require("could not read the working-copy evolution log")?;
        let mut evos: Vec<Evolution> = r
            .stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(parse_evolution)
            .collect::<Result<_, _>>()?;
        evos.reverse(); // evolog is newest-first; we want chronological
        Ok(evos)
    }

    /// Create a throwaway change without moving the working copy, then return
    /// its id. The unique description lets us identify the new change without
    /// parsing jj's human-oriented command output.
    pub fn new_no_edit(&self, parent: &str, marker: &str) -> Result<String, String> {
        self.run(&["new", "--no-edit", "-m", marker, parent])
            .require("could not create a temporary extraction change")?;
        self.changes_with_description(marker)?
            .into_iter()
            .next()
            .ok_or_else(|| "jj did not return the temporary extraction change id".to_string())
    }

    /// Overwrite `into`'s tree with `from`'s (all files), preserving `into`'s
    /// change id and description — used to update a prior extraction in place.
    pub fn restore_into(&self, into: &str, from: &str) -> Run {
        self.run(&["restore", "--from", from, "--into", into])
    }

    /// Change ids of visible commits whose description contains `needle`.
    pub fn changes_with_description(&self, needle: &str) -> Result<Vec<String>, String> {
        let pat = serde_json::to_string(needle).unwrap_or_default();
        let revset = format!("description(substring:{pat})");
        let r = self
            .run(&[
                "log",
                "-r",
                &revset,
                "--no-graph",
                "-T",
                r#"change_id.short() ++ "\n""#,
            ])
            .require("could not query changes by description")?;
        Ok(r.stdout
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect())
    }

    pub fn rebase_onto(&self, change: &str, dest: &str) -> Run {
        self.run(&["rebase", "-r", change, "-d", dest])
    }

    pub fn squash_into(&self, from: &str, into: &str) -> Run {
        self.run(&[
            "squash",
            "--from",
            from,
            "--into",
            into,
            "--use-destination-message",
        ])
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

    pub fn is_conflict(&self, rev: &str) -> Result<bool, String> {
        let r = self
            .run(&[
                "log",
                "-r",
                rev,
                "-T",
                r#"if(conflict,"1","0")"#,
                "--no-graph",
            ])
            .require("could not inspect the extracted change for conflicts")?;
        Ok(r.stdout.trim() == "1")
    }

    pub fn describe(&self, rev: &str, message: &str) -> Run {
        self.run(&["describe", "-r", rev, "-m", message])
    }
}

fn parse_evolution(line: &str) -> Result<Evolution, String> {
    let (commit, operation_user) = line
        .trim()
        .split_once(' ')
        .ok_or_else(|| format!("could not parse jj evolog output: {line:?}"))?;
    // operation.user() is "name@host". Split from the right so deliberately
    // named agents such as "team@agent" retain the complete identity.
    let user = operation_user
        .rsplit_once('@')
        .map(|(name, _host)| name)
        .unwrap_or(operation_user);
    if commit.is_empty() || user.is_empty() {
        return Err(format!("could not parse jj evolog output: {line:?}"));
    }
    Ok(Evolution {
        commit: commit.to_string(),
        user: user.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_evolution;

    #[test]
    fn parses_agent_names_containing_at_signs() {
        let evolution = parse_evolution("abc123 team@agent@workstation").unwrap();
        assert_eq!(evolution.commit, "abc123");
        assert_eq!(evolution.user, "team@agent");
    }

    #[test]
    fn rejects_unexpected_evolog_output() {
        assert!(parse_evolution("missing-user").is_err());
    }
}

//! The persisted collect state for one repo: which jj change is the stack base,
//! and which change collects each agent's edits. jj change ids are stable across
//! the rebases/squashes that `jj squash` performs, so storing them is robust.
//!
//! Read/modified/written only while holding the repo [`crate::lock::RepoLock`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentRec {
    /// The jj change id (stable id) collecting this agent's edits.
    pub change_id: String,
    /// Repo-relative POSIX paths this agent has touched (for `list`/`mine`).
    #[serde(default)]
    pub files: Vec<String>,
    /// Unix seconds of the agent's most recent edit.
    #[serde(default)]
    pub last_ts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    /// Change id of the stack floor (everything below the agents' changes,
    /// including the user's pre-existing uncommitted work). `None` until the
    /// first tracked edit establishes it.
    #[serde(default)]
    pub base: Option<String>,
    /// agent id -> its collecting change.
    #[serde(default)]
    pub agents: BTreeMap<String, AgentRec>,
}

impl State {
    fn path(base_dir: &Path) -> PathBuf {
        base_dir.join("state.json")
    }

    /// Load the state, or a default empty one if absent/corrupt.
    pub fn load(base_dir: &Path) -> State {
        match std::fs::read(Self::path(base_dir)) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => State::default(),
        }
    }

    /// Atomically persist (write to a temp file, then rename).
    pub fn save(&self, base_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(base_dir)?;
        let tmp = base_dir.join("state.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, Self::path(base_dir))
    }

    /// Record that `agent` (collecting into `change_id`) touched `paths` now.
    pub fn touch(&mut self, agent: &str, change_id: &str, paths: &[String]) {
        let rec = self.agents.entry(agent.to_string()).or_default();
        rec.change_id = change_id.to_string();
        rec.last_ts = now();
        for p in paths {
            if !rec.files.contains(p) {
                rec.files.push(p.clone());
            }
        }
        rec.files.sort();
    }
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

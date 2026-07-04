//! The persisted binding for one repo: which jj change each Claude session is
//! collecting its edits into. A session opts in by running `jj collect`; until
//! then it has no entry here and its edits are left alone.
//!
//! jj change ids are stable across the squashes/rebases involved, so storing
//! them is robust. Read/written only while holding the repo [`crate::lock::RepoLock`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    /// Change id of the stack floor — the user's pre-existing work, sealed once
    /// so it's never attributed to an agent. `None` until first established.
    #[serde(default)]
    pub base: Option<String>,
    /// Change id of the holding change: a neutral sink, just above the base,
    /// that absorbs non-tool ("foreign") edits to files an agent also edits, so
    /// they never leak into the agent's collected change.
    #[serde(default)]
    pub holding: Option<String>,
    /// session id -> the jj change id it collects into.
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
}

impl State {
    fn path(base_dir: &Path) -> PathBuf {
        base_dir.join("state.json")
    }

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
}

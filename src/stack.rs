//! Shared setup for the collect stack, used by both `jj collect` and the hook.
//!
//! The stack looks like:
//!
//! ```text
//! base ─▶ holding ─▶ change(agent-1) ─▶ change(agent-2) ─▶ @  (shared scratch)
//!  │         │            │
//!  │         │            └ only agent-1's tool-use edits
//!  │         └ non-tool ("foreign") edits, unattributed
//!  └ the user's pre-existing work, sealed
//! ```
//!
//! Agent changes are minted just beneath `@` (`insert_before @`); the holding
//! change sits at the bottom (`insert_after base`) so foreign content is always
//! an ancestor of every agent change and thus never shows up in an agent's diff.

use std::path::Path;

use crate::jj::Jj;
use crate::state::State;

/// Establish the stack floor once, sealing any pre-existing working-copy work so
/// it's never attributed to an agent. Returns the base change id.
pub fn ensure_base(jj: &Jj, state: &mut State, base_dir: &Path) -> Option<String> {
    if let Some(b) = &state.base {
        if jj.exists(b) {
            return Some(b.clone());
        }
    }
    jj.snapshot();
    // Seal the user's uncommitted work below a fresh empty `@`; agents build on
    // the empty `@`. If `@` is already empty, its parent is the floor as-is.
    if !jj.working_is_empty() && !jj.new_empty_child().ok {
        return None;
    }
    let base = jj.change_id("@-")?;
    state.base = Some(base.clone());
    let _ = state.save(base_dir);
    Some(base)
}

/// The holding change that absorbs non-tool edits. Created lazily just above the
/// base (rebasing any existing agent changes on top of it).
pub fn ensure_holding(jj: &Jj, state: &mut State, base_dir: &Path) -> Option<String> {
    if let Some(h) = &state.holding {
        if jj.exists(h) {
            return Some(h.clone());
        }
    }
    let base = ensure_base(jj, state, base_dir)?;
    let h = jj.insert_after(&base, Some("jj-collect: unattributed (non-tool edits)"))?;
    state.holding = Some(h.clone());
    let _ = state.save(base_dir);
    Some(h)
}

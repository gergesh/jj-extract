//! Extract: reconstruct each agent's isolated change from `@`'s evolog.
//!
//! Every evolution of `@` carries the agent that made it (the tagging op's
//! username). An agent's change is the composition of its evolutions' deltas —
//! for each, `diff(previous evolution, this one)` — replayed onto the base so
//! jj's 3-way merge composes the edits. Because each edit is bracketed by a
//! neutral pre-snapshot and taken under the edit lock, that per-evolution diff is
//! exactly the agent's own edit, so no file scoping is needed. Single-threaded.

use crate::jj::{Evolution, Jj};

pub struct Built {
    pub session: String,
    pub change_id: String,
    pub conflict: bool,
    /// True when this folded into a prior extraction of the same session
    /// (idempotent re-run) rather than minting a fresh change.
    pub updated: bool,
}

/// A stable, machine-readable trailer line identifying which session an extracted
/// change belongs to. It's independent of the human `-m` message, so re-running
/// `jj extract` can find its own prior output regardless of the description.
pub fn session_trailer(session: &str) -> String {
    format!("jj-extract-session: {session}")
}

/// The description put on an extracted change: the user's message (or a default
/// first line) plus the [`session_trailer`] so re-runs are idempotent.
fn extraction_desc(session: &str, message: Option<&str>) -> String {
    let body = message.map(|m| m.to_string()).unwrap_or_else(|| format!("jj-extract: {session}"));
    format!("{body}\n\n{}", session_trailer(session))
}

/// Fold a freshly-built change into any prior extraction of the same session so a
/// re-run *updates in place* instead of piling up duplicates. The prior change
/// keeps its id (and any descendants the user built on it); its content and
/// description are replaced with the fresh build, and the fresh change (plus any
/// extra stale duplicates) is abandoned. Returns `(surviving_change_id, updated)`.
pub fn reconcile_idempotent(
    jj: &Jj,
    session: &str,
    fresh: &str,
    message: Option<&str>,
) -> (String, bool) {
    let trailer = session_trailer(session);
    let priors: Vec<String> =
        jj.changes_with_description(&trailer).into_iter().filter(|c| c != fresh).collect();
    let existing = match priors.first() {
        Some(e) => e,
        None => return (fresh.to_string(), false),
    };
    jj.restore_into(existing, fresh); // existing's tree ← fresh's tree
    jj.describe(existing, &extraction_desc(session, message));
    jj.abandon(fresh);
    for extra in &priors[1..] {
        jj.abandon(extra); // collapse any earlier duplicates too
    }
    (existing.clone(), true)
}

/// The agents present in the evolog: every tagging user except the neutral one
/// (the base evolution's user, used for pre-snapshots and pre-recording state).
pub fn agents_in(evolog: &[Evolution]) -> Vec<String> {
    let neutral = evolog.first().map(|e| e.user.as_str()).unwrap_or("");
    let mut seen = std::collections::BTreeSet::new();
    for e in evolog {
        if e.user != neutral {
            seen.insert(e.user.clone());
        }
    }
    seen.into_iter().collect()
}

/// Build one agent's change from the evolog (chronological), rebased onto `base`.
///
/// The recorded evolutions all share `@`'s change id, so we must never make one a
/// graph commit (that would fork `@` into divergent versions). Instead, for each
/// evolution we build the delta on **fresh throwaway commits** whose *content* is
/// restored from `pre`/`post` — a pre-image commit, then a child holding `post`'s
/// content, so its diff-vs-parent is exactly `diff(pre, post)`. We rebase that
/// delta onto the accumulator and abandon the pre-image.
pub fn build_one(
    jj: &Jj,
    base: &str,
    session: &str,
    evolog: &[Evolution],
    message: Option<&str>,
) -> Option<Built> {
    let mut acc: Option<String> = None; // the agent's accumulating change id
    let mut scaffolds: Vec<String> = vec![]; // throwaway pre-image commits to abandon

    for i in 1..evolog.len() {
        if evolog[i].user != session {
            continue;
        }
        let pre = &evolog[i - 1].commit;
        let post = &evolog[i].commit;

        // Fresh pre-image commit on base, with pre's *content* (restore, not
        // resurrect the evolution).
        if !jj.new_on(base).ok {
            continue;
        }
        jj.restore_from(pre);
        jj.snapshot();
        let preimg = match jj.change_id("@") {
            Some(c) => c,
            None => continue,
        };
        // Fresh child holding post's content → its diff vs the pre-image is
        // exactly diff(pre, post): this agent's edit for that step.
        jj.new_empty();
        jj.restore_from(post);
        jj.snapshot();
        let delta = match jj.change_id("@") {
            Some(d) => d,
            None => continue,
        };

        match &acc {
            None => {
                jj.rebase_onto(&delta, base);
                acc = Some(delta);
            }
            Some(c) => {
                jj.rebase_onto(&delta, c);
                jj.squash_into(&delta, c);
            }
        }
        scaffolds.push(preimg);
    }

    let change = acc?;
    for s in &scaffolds {
        jj.abandon(s); // childless now (their deltas were rebased away)
    }
    jj.describe(&change, &extraction_desc(session, message));
    let conflict = jj.is_conflict(&change);
    Some(Built { session: session.to_string(), change_id: change, conflict, updated: false })
}

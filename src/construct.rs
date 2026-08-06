//! Extract: reconstruct each agent's isolated change from `@`'s evolog.
//!
//! Every evolution of `@` carries the agent that made it (the tagging op's
//! username). An agent's change is the composition of its evolutions' deltas —
//! for each, `diff(previous evolution, this one)` — replayed onto the base so
//! jj's 3-way merge composes the edits. Because each edit is bracketed by a
//! neutral pre-snapshot and taken under the edit lock, that per-evolution diff is
//! exactly the agent's own edit, so no file scoping is needed. Single-threaded.

use crate::jj::{Evolution, Jj};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_SCAFFOLD: AtomicU64 = AtomicU64::new(0);

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
    let body = message
        .map(|m| m.to_string())
        .unwrap_or_else(|| format!("jj-extract: {session}"));
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
) -> Result<(String, bool), String> {
    let trailer = session_trailer(session);
    let priors: Vec<String> = jj
        .changes_with_description(&trailer)?
        .into_iter()
        .filter(|c| c != fresh)
        .collect();
    let existing = match priors.first().cloned() {
        Some(e) => e,
        None => return Ok((fresh.to_string(), false)),
    };
    jj.restore_into(&existing, fresh)
        .require("could not update the previous extraction's content")?;
    jj.describe(&existing, &extraction_desc(session, message))
        .require("could not update the previous extraction's description")?;
    jj.abandon(fresh)
        .require("could not discard the temporary extracted change")?;
    for extra in &priors[1..] {
        jj.abandon(extra)
            .require("could not discard a duplicate prior extraction")?;
    }
    Ok((existing, true))
}

/// The agents present in the evolog: every tagging user except the neutral one
/// (the base evolution's user, used for pre-snapshots and pre-recording state).
pub fn agents_in(evolog: &[Evolution]) -> Vec<String> {
    let neutral = evolog.first().map(|e| e.user.as_str()).unwrap_or("");
    let mut agents = Vec::new();
    for e in evolog {
        if e.user != neutral && !agents.contains(&e.user) {
            agents.push(e.user.clone());
        }
    }
    agents
}

/// Arrange every extracted session as a linear stack between the original base
/// and the live working-copy change. Each extracted change still contains only
/// that session's diff, but causal edits can build on earlier sessions and the
/// repository is left with one tool-created head instead of one sibling head per
/// extraction.
pub fn stack_extractions(
    jj: &Jj,
    base: &str,
    live: &str,
    evolog: &[Evolution],
) -> Result<(), String> {
    let mut tip = base.to_string();
    for session in agents_in(evolog) {
        let trailer = session_trailer(&session);
        let Some(change) = jj.changes_with_description(&trailer)?.into_iter().next() else {
            continue;
        };
        if change != tip {
            jj.rebase_onto(&change, &tip)
                .require("could not stack an extracted session change")?;
        }
        tip = change;
    }
    if tip != base {
        jj.rebase_onto(live, &tip)
            .require("could not place the live working copy on the extraction stack")?;
    }
    Ok(())
}

/// Build one agent's change from the evolog (chronological), rebased onto `base`.
///
/// The recorded evolutions all share `@`'s change id, so we must never make one a
/// graph commit (that would fork `@` into divergent versions). Instead, for each
/// evolution we build the delta on **fresh throwaway commits** created with
/// `--no-edit`, so the live working-copy change is never left or auto-abandoned.
/// Their *content* is restored from `pre`/`post` — a pre-image commit, then a
/// child holding `post`'s content — making the child's diff exactly
/// `diff(pre, post)`. We rebase that delta onto the accumulator and abandon the
/// pre-image.
pub fn build_one(
    jj: &Jj,
    base: &str,
    session: &str,
    evolog: &[Evolution],
    message: Option<&str>,
) -> Result<Option<Built>, String> {
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
        let preimg = jj.new_no_edit(base, &scaffold_marker("pre"))?;
        jj.restore_into(&preimg, pre)
            .require("could not restore an edit's pre-image")?;
        // Fresh child holding post's content → its diff vs the pre-image is
        // exactly diff(pre, post): this agent's edit for that step.
        let delta = jj.new_no_edit(&preimg, &scaffold_marker("post"))?;
        jj.restore_into(&delta, post)
            .require("could not restore an edit's post-image")?;

        match &acc {
            None => {
                jj.rebase_onto(&delta, base)
                    .require("could not rebase the first recorded edit onto the base")?;
                acc = Some(delta);
            }
            Some(c) => {
                jj.rebase_onto(&delta, c)
                    .require("could not compose a recorded edit onto the extraction")?;
                jj.squash_into(&delta, c)
                    .require("could not combine a recorded edit with the extraction")?;
            }
        }
        scaffolds.push(preimg);
    }

    let Some(change) = acc else {
        return Ok(None);
    };
    for s in &scaffolds {
        jj.abandon(s)
            .require("could not clean up a temporary extraction change")?;
    }
    jj.describe(&change, &extraction_desc(session, message))
        .require("could not describe the extracted change")?;
    let conflict = jj.is_conflict(&change)?;
    Ok(Some(Built {
        session: session.to_string(),
        change_id: change,
        conflict,
        updated: false,
    }))
}

fn scaffold_marker(kind: &str) -> String {
    let sequence = NEXT_SCAFFOLD.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!(
        "jj-extract temporary {kind} {} {timestamp} {sequence}",
        std::process::id()
    )
}

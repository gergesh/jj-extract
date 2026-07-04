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
pub fn build_one(
    jj: &Jj,
    base: &str,
    session: &str,
    evolog: &[Evolution],
    message: Option<&str>,
) -> Option<Built> {
    let mut acc: Option<String> = None; // the agent's accumulating change id

    for i in 1..evolog.len() {
        if evolog[i].user != session {
            continue;
        }
        let pre = &evolog[i - 1].commit;
        let post = &evolog[i].commit;

        // A delta commit whose diff-vs-parent is exactly diff(pre, post): put `@`
        // on `pre`, restore it to `post`'s tree, snapshot.
        if !jj.new_on(pre).ok {
            continue;
        }
        jj.restore_from(post);
        jj.snapshot();
        let delta = match jj.change_id("@") {
            Some(d) => d,
            None => continue,
        };
        jj.new_empty(); // move @ off the delta so the rebase is clean

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
    }

    let change = acc?;
    let desc = message.map(|m| m.to_string()).unwrap_or_else(|| format!("jj-extract: {session}"));
    jj.describe(&change, &desc);
    let conflict = jj.is_conflict(&change);
    Some(Built { session: session.to_string(), change_id: change, conflict })
}

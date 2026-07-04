//! Harvest: reconstruct each agent's isolated change from recorded snapshots.
//!
//! This is the single-threaded half of the design — one process runs `--build`,
//! so there's no concurrency and none of the locking that live stack-mutation
//! needed. For each of an agent's recorded tool calls we rebuild a **delta
//! commit** (its `pre` snapshot with the edited files swapped to their `post`
//! content) and rebase/squash it onto the agent's accumulating change, letting
//! jj's 3-way merge compose the edits — the same content math validated live,
//! just replayed from the record.

use std::collections::BTreeSet;

use crate::jj::Jj;
use crate::store::Event;

pub struct Built {
    pub session: String,
    pub change_id: String,
    pub files: usize,
    pub conflict: bool,
}

/// Build one agent's change from its events (in recorded order), rebased onto
/// `base`. Returns None if the agent recorded nothing.
pub fn build_one(
    jj: &Jj,
    base: &str,
    session: &str,
    events: &[&Event],
    message: Option<&str>,
) -> Option<Built> {
    let mut acc: Option<String> = None; // the agent's accumulating change id
    let mut touched: BTreeSet<String> = BTreeSet::new();

    for e in events {
        // A delta commit: check out the pre snapshot, swap the edited files to
        // their post content, snapshot. Its diff vs `pre` is exactly this tool
        // use's change to those files.
        if !jj.new_on(&e.pre).ok {
            continue;
        }
        for f in &e.files {
            touched.insert(f.clone());
            let path = jj.root_path().join(f);
            match jj.file_show(&e.post, f) {
                Some(content) => {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::write(&path, content);
                }
                None => {
                    let _ = std::fs::remove_file(&path); // deleted in post
                }
            }
        }
        jj.file_track(&e.files);
        jj.snapshot();
        let delta = match jj.change_id("@") {
            Some(d) => d,
            None => continue,
        };
        jj.new_empty(); // move @ off the delta so the rebase is clean

        match &acc {
            None => {
                // First delta becomes the agent's change, lifted onto base.
                jj.rebase_onto(&delta, base);
                acc = Some(delta);
            }
            Some(c) => {
                // Apply this delta on top of the accumulator, then fold it in.
                jj.rebase_onto(&delta, c);
                jj.squash_into(&delta, c);
            }
        }
    }

    let change = acc?;
    if let Some(m) = message {
        jj.describe(&change, m);
    }
    let conflict = jj.is_conflict(&change);
    Some(Built { session: session.to_string(), change_id: change, files: touched.len(), conflict })
}

/// Build the requested sessions, restoring the live working copy afterwards.
/// `orig` is the working-copy commit captured before building (the live,
/// all-agents state); we return `@` to it so harvesting doesn't disturb editing.
pub fn build_all(
    jj: &Jj,
    base: &str,
    sessions: &[(String, Option<String>)], // (session, message)
    events: &[Event],
    orig: Option<&str>,
) -> Vec<Built> {
    let mut out = vec![];
    for (session, message) in sessions {
        let evs: Vec<&Event> = events.iter().filter(|e| &e.session == session).collect();
        if let Some(b) = build_one(jj, base, session, &evs, message.as_deref()) {
            out.push(b);
        }
    }
    // Put the live working copy back where it was.
    if let Some(o) = orig {
        let _ = jj.new_on(o);
    }
    out
}

//! Extract recorded agent edits in one jj transaction.
//!
//! Pending attributed evolutions contribute their predecessor-to-snapshot deltas.
//! Formatter-only edits and neutral snapshots are carried on the paths that
//! edit touched, then removed again when their inverse commutes cleanly. We
//! compose those deltas in memory, rewrite the extracted stack and the live
//! working-copy commit once, then publish one operation. This keeps the operation
//! log as the attribution ledger while making one `jj undo` reverse a complete
//! extraction. A dry run prepares the same transaction, including descendant
//! rebases and tree checks, but publishes no operation or working-copy update.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::default_backend_factories::{
    default_backend_factories, default_working_copy_factories,
};
use jj_lib::matchers::EverythingMatcher;
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_walk;
use jj_lib::operation::Operation;
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::revset::{RevsetExpression, RevsetStreamExt as _};
use jj_lib::rewrite::{RebaseOptions, RebasedCommit};
use jj_lib::settings::UserSettings;
use jj_lib::store::Store;
use jj_lib::workspace::Workspace;
use serde::{Deserialize, Serialize};

use crate::identity;
use crate::jj::{Evolution, Jj};

pub struct Built {
    pub session: String,
    /// The change the session's edits landed in. `None` only for a dry run that
    /// would create one: a speculative preview ID is not a published change ID.
    pub change_id: Option<String>,
    pub conflict: bool,
    /// True when this updated a prior extraction of the same session.
    pub updated: bool,
    /// Paths the change contains, relative to its parent. Collected for a dry
    /// run, whose report is the only view of a result that is never written;
    /// after a real extraction `jj show` is that view, so it stays empty.
    pub files: Vec<String>,
    /// The preceding session in the chosen stack, or the original base.
    pub after_session: Option<String>,
}

#[derive(Clone, Copy)]
pub struct ExtractOptions {
    pub dry_run: bool,
    pub allow_conflicts: bool,
    pub squash: bool,
}

#[derive(Default)]
pub struct Extraction {
    pub changes: Vec<Built>,
    pub descendant_conflicts: Vec<String>,
}

/// Operation attribute holding this tool's session -> extracted changes ledger.
///
/// The mapping lives in the extraction operation's own metadata rather than in
/// a commit description. A description belongs to whoever writes it: the moment
/// someone runs `jj describe`, machine state kept there is gone, and until then
/// it is noise in every log, blame and pull request. jj's operation log is
/// already this tool's ledger, and it survives the rewrites that extraction and
/// re-description perform on the change itself.
const LEDGER_ATTRIBUTE: &str = "jj-extract.extractions";
const PROGRESS_ATTRIBUTE: &str = "jj-extract.progress";

/// Snapshot IDs, not timestamps or descriptions, identify consumed edits. The
/// operation log owns the checkpoint, so undo restores the pending edits too.
#[derive(Default, Serialize, Deserialize)]
struct Progress {
    consumed: BTreeSet<String>,
    known_changes: BTreeMap<String, Checkpoint>,
}

#[derive(Serialize, Deserialize)]
struct Checkpoint {
    snapshots: BTreeSet<String>,
    commit: String,
}

/// Session -> the changes extracted for it, newest last. A session can hold
/// more than one: each incremental extraction adds a new chunk.
type Ledger = BTreeMap<String, Vec<String>>;

/// The description a *newly* built change starts with: a placeholder naming the
/// session, for the author to replace with `jj describe` once they can see what
/// the change contains. Extraction deliberately writes no prose of its own.
fn extraction_desc(session: &str, kind: Option<&str>) -> String {
    let body = format!("jj-extract: {session}");
    // The standard trailer each agent already writes for its own commits, so
    // existing co-authorship tooling reads an extracted change unaided.
    match kind.and_then(identity::coauthor) {
        Some(coauthor) => format!("{body}\n\nCo-authored-by: {coauthor}"),
        None => body,
    }
}

/// The agent product behind `session`, from the first of its evolutions that
/// recorded one.
fn kind_of(evolog: &[Evolution], session: &str) -> Option<String> {
    evolog
        .iter()
        .find(|evolution| evolution.user == session && evolution.kind.is_some())
        .and_then(|evolution| evolution.kind.clone())
}

/// The newest ledger this tool published, found by walking back through its own
/// operations. Missing metadata starts empty; unreadable metadata must fail
/// rather than risk replaying already extracted edits.
async fn read_ledger(
    repo: &ReadonlyRepo,
    workspace: &jj_lib::ref_name::WorkspaceName,
    evolog: &[Evolution],
) -> Result<(Ledger, Progress), String> {
    use futures::TryStreamExt as _;

    let head: Operation = repo.operation().clone();
    let mut operations = Box::pin(op_walk::walk_ancestors(&[head]));
    while let Some(operation) = operations
        .try_next()
        .await
        .map_err(|e| format!("could not read the operation log: {e}"))?
    {
        if let Some(raw) = operation.metadata().attributes.get(LEDGER_ATTRIBUTE) {
            // `jj undo` restores a view, but the undone operation remains in
            // the operation ancestry. Only checkpoints on this live change's
            // surviving evolution history have actually consumed its edits.
            let view = operation
                .view()
                .await
                .map_err(|e| format!("could not read extraction checkpoint view: {e}"))?;
            if !view
                .get_wc_commit_id(workspace)
                .is_some_and(|id| evolog.iter().any(|evolution| evolution.commit == id.hex()))
            {
                continue;
            }
            let ledger = serde_json::from_str(raw)
                .map_err(|e| format!("could not read the extraction ledger: {e}"))?;
            let progress = operation
                .metadata()
                .attributes
                .get(PROGRESS_ATTRIBUTE)
                .map(|raw| serde_json::from_str(raw))
                .transpose()
                .map_err(|e| format!("could not read extraction progress: {e}"))?
                .unwrap_or_default();
            return Ok((ledger, progress));
        }
    }
    Ok((Ledger::new(), Progress::default()))
}

/// Agents present in the evolog, in first-edit order.
pub fn agents_in(evolog: &[Evolution]) -> Vec<String> {
    let neutral = evolog.first().map(|e| e.user.as_str()).unwrap_or("");
    let mut agents = Vec::new();
    for evolution in evolog {
        if evolution.user != neutral && !agents.contains(&evolution.user) {
            agents.push(evolution.user.clone());
        }
    }
    agents
}

pub fn extract(
    root: &Path,
    jj: &Jj,
    evolog: &[Evolution],
    targets: &[String],
    options: ExtractOptions,
) -> Result<Extraction, String> {
    let sessions = agents_in(evolog);
    let settings = jj.settings(
        evolog
            .first()
            .map(|e| e.user.as_str())
            .unwrap_or("jj-extract"),
    )?;
    pollster::block_on(extract_async(
        root,
        evolog,
        targets,
        &sessions,
        settings.user_name(),
        settings.user_email(),
        options,
    ))
}

async fn extract_async(
    root: &Path,
    evolog: &[Evolution],
    targets: &[String],
    sessions: &[String],
    user_name: &str,
    user_email: &str,
    options: ExtractOptions,
) -> Result<Extraction, String> {
    let ExtractOptions {
        dry_run,
        allow_conflicts,
        squash,
    } = options;
    let neutral = evolog
        .first()
        .map(|e| e.user.as_str())
        .ok_or_else(|| "nothing recorded yet".to_string())?;
    let settings = extraction_settings(neutral, user_name, user_email)?;
    let mut workspace = Workspace::load(
        &settings,
        root,
        &default_backend_factories(),
        &default_working_copy_factories(),
    )
    .map_err(|e| format!("could not load the jj workspace: {e}"))?;
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .map_err(|e| format!("could not load the jj repository: {e}"))?;
    let store = repo.store();
    let workspace_name = workspace.workspace_name().to_owned();
    let live_id = repo
        .view()
        .get_wc_commit_id(&workspace_name)
        .cloned()
        .ok_or_else(|| "could not resolve the live working-copy commit".to_string())?;
    let live = store
        .get_commit(&live_id)
        .map_err(|e| format!("could not load the live working-copy commit: {e}"))?;
    let old_live_tree = live.tree();
    // Hold jj's native working-copy lock throughout planning and publication.
    // The hook lock is time-bounded and may expire during an expensive search.
    let mut locked_workspace = workspace
        .start_working_copy_mutation()
        .await
        .map_err(|e| format!("could not lock the live working copy: {e}"))?;
    verify_live_tree(
        &old_live_tree,
        locked_workspace.locked_wc().old_tree(),
        "before extraction",
    )?;

    let oldest = load_commit(store, &evolog[0].commit)?;
    let base_id = oldest
        .parent_ids()
        .first()
        .cloned()
        .ok_or_else(|| "the oldest recorded evolution has no parent".to_string())?;
    let original_base = store
        .get_commit(&base_id)
        .map_err(|e| format!("could not load the extraction base: {e}"))?;

    let (mut ledger, mut progress) = read_ledger(repo.as_ref(), &workspace_name, evolog).await?;
    let existing = existing_extractions(repo.as_ref(), &original_base, sessions, &ledger).await?;
    // Appending uses the current parent tree, preserving earlier extracted
    // commits byte-for-byte. Only --squash rebuilds/reorders the owned stack.
    let base_tree = if squash {
        original_base.tree()
    } else {
        live.parent_tree(repo.as_ref())
            .await
            .map_err(|e| format!("could not read the current parent tree: {e}"))?
    };
    let base_parents = if squash {
        vec![original_base.id().clone()]
    } else {
        live.parent_ids().to_vec()
    };

    let mut plans = Vec::new();
    let unconsumed = BTreeSet::new();
    for session in sessions {
        let selected = targets.contains(session);
        if !squash && !selected {
            continue;
        }
        let mut priors = existing.get(session).cloned().unwrap_or_default();
        let changes = ledger.get(session).cloned().unwrap_or_default();
        priors.sort_by_key(|commit| {
            changes
                .iter()
                .position(|id| *id == commit.change_id().hex())
        });
        if squash
            && priors
                .windows(2)
                .any(|pair| pair[0].change_id() == pair[1].change_id())
        {
            return Err(format!(
                "session {session} has divergent extracted changes; resolve the divergence first"
            ));
        }
        let latest = priors.last();
        let legacy = latest.is_some_and(|commit| {
            !progress
                .known_changes
                .contains_key(&commit.change_id().hex())
        });
        if selected && legacy && !squash {
            return Err(format!(
                "session {session} has an older extraction without an edit checkpoint. \
                 Run --squash once to establish its checkpoint before creating incremental extractions"
            ));
        }
        let consumed = if legacy && squash {
            &unconsumed
        } else {
            &progress.consumed
        };
        let snapshots = if selected {
            evolog
                .iter()
                .filter(|e| e.user == *session && !consumed.contains(&e.commit))
                .map(|e| e.commit.clone())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let pending = if selected {
            recorded_edits(store, &base_tree, session, evolog, consumed).await?
        } else {
            Vec::new()
        };
        if !squash {
            if !pending.is_empty() {
                plans.push(SessionPlan {
                    session: session.clone(),
                    edits: pending,
                    fallback: None,
                    prior: None,
                    snapshots,
                    selected: true,
                });
            }
            continue;
        }
        let latest_id = latest.map(|commit| commit.id().clone());
        let mut pending = pending;
        for prior in priors {
            let update = selected && Some(prior.id()) == latest_id.as_ref();
            let mut fallback = None;
            if update {
                if let Some(checkpoint) = progress.known_changes.get(&prior.change_id().hex()) {
                    let recorded = load_commit(store, &checkpoint.commit)?;
                    // A squashed delta can lose useful formatter context.
                    // Replaying this chunk's original edits is another exact
                    // representation, provided no manual content was added.
                    if recorded.parent_ids() == prior.parent_ids()
                        && recorded.tree().tree_ids_and_labels()
                            == prior.tree().tree_ids_and_labels()
                    {
                        let earlier_chunks = consumed
                            .difference(&checkpoint.snapshots)
                            .cloned()
                            .collect();
                        fallback = Some(
                            recorded_edits(store, &base_tree, session, evolog, &earlier_chunks)
                                .await?,
                        );
                    }
                }
            }
            let mut edits = if update && legacy {
                Vec::new()
            } else {
                vec![ReplayEdit {
                    before: prior.parent_tree(repo.as_ref()).await.map_err(|e| {
                        format!("could not read {session}'s prior parent tree: {e}")
                    })?,
                    after: prior.tree(),
                    neutral: false,
                }]
            };
            if update {
                edits.append(&mut pending);
            }
            plans.push(SessionPlan {
                session: session.clone(),
                edits,
                fallback,
                prior: Some(prior),
                snapshots: if update {
                    snapshots.clone()
                } else {
                    Vec::new()
                },
                selected: update,
            });
        }
        if latest_id.is_none() && !pending.is_empty() {
            plans.push(SessionPlan {
                session: session.clone(),
                edits: pending,
                fallback: None,
                prior: None,
                snapshots,
                selected,
            });
        }
    }
    if !plans.iter().any(|plan| plan.selected) {
        return Ok(Extraction::default());
    }
    let stack = plan_stack(&base_tree, &plans).await?;
    if !dry_run && !allow_conflicts && stack.conflicts > 0 {
        let sessions = stack
            .order
            .iter()
            .zip(&stack.trees)
            .filter(|(_, tree)| tree.has_conflict())
            .map(|(&index, _)| plans[index].session.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "extraction would create conflicts in session(s) {sessions}; repository unchanged. \
             Inspect with --dry-run, or pass --allow-conflicts to proceed"
        ));
    }

    // The transaction is the unit of publication: nothing inside it reaches the
    // repository until the `tx.commit()` below, which a dry run never makes.
    let mut tx = repo.start_transaction();
    tx.set_workspace_name(&workspace_name);
    // Dry runs also build unreferenced commits inside this transaction so that
    // descendant rebasing and tree verification run exactly as in a real extract.
    let mut tip_parents = base_parents;
    let mut tip_tree = base_tree;
    let mut built = Vec::new();

    for (position, (&index, stacked_tree)) in stack.order.iter().zip(&stack.trees).enumerate() {
        let plan = &plans[index];
        let session = &plan.session;
        let prior = &plan.prior;
        // Reordering and re-extraction preserve the author's description.
        let description = prior
            .as_ref()
            .map(|commit| commit.description().to_owned())
            .unwrap_or_else(|| extraction_desc(session, kind_of(evolog, session).as_deref()));
        let updated = prior.is_some();
        let result = match prior {
            Some(prior)
                if prior.parent_ids() == tip_parents
                    && prior.tree().tree_ids_and_labels() == stacked_tree.tree_ids_and_labels() =>
            {
                Ok(prior.clone())
            }
            Some(prior) => {
                tx.repo_mut()
                    .rewrite_commit(prior)
                    .set_parents(tip_parents.clone())
                    .set_tree(stacked_tree.clone())
                    .set_description(description)
                    .write()
                    .await
            }
            None => {
                tx.repo_mut()
                    .new_commit(tip_parents.clone(), stacked_tree.clone())
                    .set_description(description)
                    .write()
                    .await
            }
        };
        let commit =
            result.map_err(|e| format!("could not write extracted session {session}: {e}"))?;
        if plan.selected {
            let extracted = ledger.entry(session.clone()).or_default();
            let change = commit.change_id().hex();
            if !extracted.contains(&change) {
                extracted.push(change.clone());
            }
            let checkpoint = progress
                .known_changes
                .entry(change)
                .or_insert_with(|| Checkpoint {
                    snapshots: BTreeSet::new(),
                    commit: String::new(),
                });
            checkpoint.snapshots.extend(plan.snapshots.iter().cloned());
            checkpoint.commit = commit.id().hex();
            progress.consumed.extend(plan.snapshots.iter().cloned());
        }
        // Existing entries are rewritten too. Report the entire chosen stack,
        // including conflicts in a non-target entry that moved above a target.
        built.push(Built {
            session: session.clone(),
            // Name existing changes in a preview, but do not expose speculative
            // IDs which will differ when a real extraction creates the change.
            change_id: (if dry_run {
                prior.as_ref()
            } else {
                Some(&commit)
            })
            .map(|commit| short_hex(commit.change_id().reverse_hex())),
            conflict: stacked_tree.has_conflict(),
            updated,
            files: if dry_run {
                changed_file_names(&tip_tree, stacked_tree).await?
            } else {
                Vec::new()
            },
            after_session: position
                .checked_sub(1)
                .map(|previous| plans[stack.order[previous]].session.clone()),
        });
        tip_parents = vec![commit.id().clone()];
        tip_tree = stacked_tree.clone();
    }

    let new_live = tx
        .repo_mut()
        .rewrite_commit(&live)
        .set_parents(tip_parents)
        .set_tree(old_live_tree.clone())
        .write()
        .await
        .map_err(|e| {
            format!("could not place the live working copy on the extraction stack: {e}")
        })?;
    tx.repo_mut()
        .set_wc_commit(workspace_name.clone(), new_live.id().clone())
        .map_err(|e| format!("could not preserve the live working-copy change: {e}"))?;
    let mut rebased_descendants = Vec::new();
    tx.repo_mut()
        .rebase_descendants_with_options(
            &RevsetExpression::none(),
            &RebaseOptions::default(),
            |old, rebased| {
                if let RebasedCommit::Rewritten(commit) = rebased {
                    if commit.tree().has_conflict() {
                        rebased_descendants.push((old, commit));
                    }
                }
            },
        )
        .await
        .map_err(|e| format!("could not rebase descendants of extracted changes: {e}"))?;
    let mut descendant_conflicts = Vec::new();
    for (old, rebased) in rebased_descendants {
        for (path, value) in rebased.tree().conflicts() {
            let value = value.map_err(|e| format!("could not inspect descendant conflict: {e}"))?;
            let old_value = old
                .tree()
                .path_value(&path)
                .await
                .map_err(|e| format!("could not inspect prior descendant tree: {e}"))?;
            if value != old_value {
                descendant_conflicts.push(short_hex(rebased.change_id().reverse_hex()));
                break;
            }
        }
    }
    if !dry_run && !allow_conflicts && !descendant_conflicts.is_empty() {
        return Err(format!(
            "extraction would create conflicts in descendant change(s) {}; repository unchanged. \
             Pass --allow-conflicts to proceed",
            descendant_conflicts.join(", ")
        ));
    }
    // Check the actual workspace commit selected by the completed transaction,
    // after descendant rebasing, rather than trusting set_tree() above.
    let prepared_id = tx
        .repo()
        .view()
        .get_wc_commit_id(&workspace_name)
        .ok_or_else(|| "the extraction transaction lost the working-copy commit".to_string())?;
    let prepared = store
        .get_commit(prepared_id)
        .map_err(|e| format!("could not load the prepared working-copy commit: {e}"))?;
    verify_live_tree(
        &old_live_tree,
        &prepared.tree(),
        "before publishing extraction",
    )?;

    let extraction = Extraction {
        changes: built,
        descendant_conflicts,
    };
    // All safety checks have run. Dropping a preview transaction publishes no
    // operation and leaves speculative objects unreachable from the repository.
    if dry_run {
        return Ok(extraction);
    }

    tx.set_attribute(
        LEDGER_ATTRIBUTE.to_string(),
        serde_json::to_string(&ledger)
            .map_err(|e| format!("could not write the extraction ledger: {e}"))?,
    );
    tx.set_attribute(
        PROGRESS_ATTRIBUTE.to_string(),
        serde_json::to_string(&progress)
            .map_err(|e| format!("could not write extraction progress: {e}"))?,
    );
    let operation_description = if extraction.changes.len() == 1 {
        format!("extract session {}", extraction.changes[0].session)
    } else {
        format!("extract {} sessions", extraction.changes.len())
    };
    let new_repo = tx
        .commit(operation_description)
        .await
        .map_err(|e| format!("could not commit the extraction transaction: {e}"))?;
    let checked_out_id = new_repo
        .view()
        .get_wc_commit_id(&workspace_name)
        .ok_or_else(|| "the extraction transaction lost the working-copy commit".to_string())?;
    let checked_out = new_repo
        .store()
        .get_commit(checked_out_id)
        .map_err(|e| format!("could not load the extracted working-copy commit: {e}"))?;
    verify_live_tree(
        &old_live_tree,
        &checked_out.tree(),
        "after publishing extraction",
    )?;
    let stats = locked_workspace
        .locked_wc()
        .check_out(&checked_out)
        .await
        .map_err(|e| format!("could not update the working copy after extraction: {e}"))?;
    if stats != jj_lib::working_copy::CheckoutStats::default() {
        return Err(format!(
            "live tree verification failed: checkout unexpectedly changed files: {stats:?}"
        ));
    }
    locked_workspace
        .finish(new_repo.op_id().clone())
        .await
        .map_err(|e| format!("could not save the verified working-copy state: {e}"))?;

    Ok(extraction)
}

fn verify_live_tree(before: &MergedTree, after: &MergedTree, phase: &str) -> Result<(), String> {
    if before.tree_ids_and_labels() != after.tree_ids_and_labels() {
        return Err(format!(
            "live tree verification failed {phase}: tree contents or conflict labels changed"
        ));
    }
    Ok(())
}

/// The changes a previous extraction built for `sessions`, restricted to this
/// working-copy line. The ledger names them by change id, which survives every
/// rewrite extraction performs — and every `jj describe` the user performs.
async fn existing_extractions(
    repo: &dyn jj_lib::repo::Repo,
    base: &Commit,
    sessions: &[String],
    ledger: &Ledger,
) -> Result<HashMap<String, Vec<Commit>>, String> {
    use futures::TryStreamExt as _;

    // A session can have extracted changes on another visible branch. Only a
    // change descended from this working-copy line's stable extraction base is a
    // candidate for in-place update or legacy-stack linearization.
    let descendants = RevsetExpression::commits(vec![base.id().clone()]).descendants();
    let expression = descendants.intersection(&RevsetExpression::all());
    let revset = expression
        .evaluate(repo)
        .map_err(|e| format!("could not evaluate existing extracted changes: {e}"))?;
    let commits: Vec<_> = revset
        .stream()
        .commits(repo.store())
        .try_collect()
        .await
        .map_err(|e| format!("could not read existing extracted changes: {e}"))?;

    let mut existing: HashMap<String, Vec<Commit>> = HashMap::new();
    for commit in commits {
        let change = commit.change_id().hex();
        for session in sessions {
            if ledger
                .get(session)
                .is_some_and(|changes| changes.contains(&change))
            {
                existing
                    .entry(session.clone())
                    .or_default()
                    .push(commit.clone());
            }
        }
    }
    Ok(existing)
}

struct ReplayEdit {
    before: MergedTree,
    after: MergedTree,
    neutral: bool,
}

struct SessionPlan {
    session: String,
    edits: Vec<ReplayEdit>,
    fallback: Option<Vec<ReplayEdit>>,
    prior: Option<Commit>,
    snapshots: Vec<String>,
    selected: bool,
}

/// Load history once, before trying alternative placements. Neutral snapshots
/// are relevant even when another session edited an unrelated file in between.
/// Walking backwards lets us carry only paths this session will subsequently
/// touch, and excludes formatter sweeps after its last edit.
async fn recorded_edits(
    store: &Arc<Store>,
    base: &MergedTree,
    session: &str,
    evolog: &[Evolution],
    consumed: &BTreeSet<String>,
) -> Result<Vec<ReplayEdit>, String> {
    let neutral = evolog.first().map(|e| e.user.as_str()).unwrap_or("");
    let mut future_paths = BTreeSet::new();
    let mut edits = Vec::new();
    if !evolog
        .iter()
        .skip(1)
        .any(|evolution| evolution.user == session && !consumed.contains(&evolution.commit))
    {
        return Ok(edits);
    }
    // Formatting may predate the last extracted edit on another path. Keep
    // that optional context available when a later chunk first touches it.
    for index in (1..evolog.len()).rev() {
        let evolution = &evolog[index];
        if evolution.user == session && consumed.contains(&evolution.commit) {
            // The parent (or updated prior chunk) already carries this edit's
            // context. Do not replay older formatting on those paths over it.
            // Other paths may still need formatting from before this snapshot.
            let before = load_commit(store, &evolog[index - 1].commit)?;
            let after = load_commit(store, &evolution.commit)?;
            let paths = changed_paths(&before.tree(), &after.tree()).await?;
            let formatting =
                crate::formatting::equivalent_paths(&before.tree(), &after.tree(), &paths).await?;
            for path in paths.difference(&formatting) {
                future_paths.remove(path);
            }
            continue;
        }
        let is_neutral = evolution.user == neutral && evolution.is_snapshot;
        if evolution.user != session && (!evolution.is_snapshot || future_paths.is_empty()) {
            continue;
        }
        let before = load_commit(store, &evolog[index - 1].commit)?;
        let after = load_commit(store, &evolution.commit)?;
        let paths = changed_paths(&before.tree(), &after.tree()).await?;
        let optional = if is_neutral {
            paths.clone()
        } else {
            crate::formatting::equivalent_paths(&before.tree(), &after.tree(), &paths).await?
        };
        if evolution.user == session {
            let owned: BTreeSet<_> = paths.difference(&optional).cloned().collect();
            if !owned.is_empty() {
                future_paths.extend(owned.iter().cloned());
                edits.push(ReplayEdit {
                    before: paths_from(base, &before.tree(), &owned).await?,
                    after: paths_from(base, &after.tree(), &owned).await?,
                    neutral: false,
                });
            }
        }
        let overlapping = optional.intersection(&future_paths).cloned().collect();
        if !BTreeSet::is_empty(&overlapping) {
            edits.push(ReplayEdit {
                before: paths_from(base, &before.tree(), &overlapping).await?,
                after: paths_from(base, &after.tree(), &overlapping).await?,
                neutral: true,
            });
        }
    }
    edits.reverse();
    Ok(edits)
}

async fn replay_session(parent: &MergedTree, plan: &SessionPlan) -> Result<MergedTree, String> {
    let result = replay_edits(parent, &plan.session, &plan.edits).await?;
    if result.has_conflict() {
        if let Some(edits) = &plan.fallback {
            let replayed = replay_edits(parent, &plan.session, edits).await?;
            if replayed.conflicts().count() < result.conflicts().count() {
                return Ok(replayed);
            }
        }
    }
    Ok(result)
}

async fn replay_edits(
    parent: &MergedTree,
    session: &str,
    edits: &[ReplayEdit],
) -> Result<MergedTree, String> {
    let mut result = parent.clone();
    let mut carried = Vec::new();
    for edit in edits {
        let merged = apply_delta(&result, &edit.before, &edit.after)
            .await
            .map_err(|e| format!("could not replay an edit for {session}: {e}"))?;
        let next = if edit.neutral && merged.has_conflict() {
            crate::neutral::keep_clean_hunks(&result, &edit.before, &edit.after, &merged).await?
        } else {
            merged
        };
        if edit.neutral {
            // Undo only context actually introduced here. The parent may
            // already own some or all of the neutral rewrite.
            carried.push((result, next.clone()));
        }
        result = next;
    }
    for (before, after) in carried.iter().rev() {
        let candidate = apply_delta(&result, after, before)
            .await
            .map_err(|e| format!("could not remove neutral context for {session}: {e}"))?;
        // Retain only the paths whose inverse conflicts. One adopted rewrite
        // must not drag unrelated neutral edits in another file along with it.
        let mut removable = changed_paths(&result, &candidate).await?;
        for (path, value) in candidate.conflicts() {
            value.map_err(|e| format!("could not inspect neutral-context conflict: {e}"))?;
            removable.remove(&path);
        }
        result = paths_from(&result, &candidate, &removable).await?;
    }
    if result.has_conflict() && !carried.is_empty() {
        // Context is optional. A formatter can depend on an omitted agent even
        // when this session's actual edits commute without the formatting.
        let mut without_context = parent.clone();
        for edit in edits.iter().filter(|edit| !edit.neutral) {
            without_context = apply_delta(&without_context, &edit.before, &edit.after)
                .await
                .map_err(|e| format!("could not replay without neutral context: {e}"))?;
        }
        // Choose per path, so optional formatting in one file cannot mask a
        // necessary rewrite in another. Never replace a clean result with a
        // conflicted one merely to preserve formatting.
        let mut recoverable = BTreeSet::new();
        for (path, value) in result.conflicts() {
            value.map_err(|e| format!("could not inspect replay conflict: {e}"))?;
            if without_context
                .path_value(&path)
                .await
                .map_err(|e| format!("could not inspect replay without context: {e}"))?
                .is_resolved()
            {
                recoverable.insert(path);
            }
        }
        result = paths_from(&result, &without_context, &recoverable).await?;
    }
    Ok(result)
}

#[derive(Clone, Default)]
struct StackPlan {
    order: Vec<usize>,
    trees: Vec<MergedTree>,
    /// Count every conflicted path at every stack entry, not just at the tip:
    /// a later edit can resolve a conflict while leaving its parent broken.
    conflicts: usize,
}

impl StackPlan {
    async fn append(
        &self,
        base: &MergedTree,
        plans: &[SessionPlan],
        index: usize,
    ) -> Result<Self, String> {
        let tree = replay_session(self.trees.last().unwrap_or(base), &plans[index]).await?;
        let mut result = self.clone();
        for (_, value) in tree.conflicts() {
            value.map_err(|e| format!("could not inspect planned conflict: {e}"))?;
            result.conflicts += 1;
        }
        result.order.push(index);
        result.trees.push(tree);
        Ok(result)
    }
}

/// Chronology is a cheap, deterministic preference, not a placement constraint.
/// On conflict, search several prefixes in parallel so a locally clean choice
/// cannot immediately lock us into a bad order. Replay at the actual candidate
/// parent: reconstructing against the base first loses the dependency context.
/// Work is bounded; a difficult cycle must not make extraction factorial.
async fn plan_stack(base: &MergedTree, plans: &[SessionPlan]) -> Result<StackPlan, String> {
    const BEAM_WIDTH: usize = 32;
    const MAX_REPLAYS: usize = 4096;

    let mut best = StackPlan::default();
    for index in 0..plans.len() {
        best = best.append(base, plans, index).await?;
    }
    if best.conflicts == 0 || plans.len() < 2 {
        return Ok(best);
    }

    let mut beam = vec![StackPlan::default()];
    let mut replays = 0;
    for _ in 0..plans.len() {
        let mut next = Vec::new();
        for prefix in &beam {
            for index in 0..plans.len() {
                if prefix.order.contains(&index)
                    || (0..index).any(|earlier| {
                        plans[earlier].session == plans[index].session
                            && !prefix.order.contains(&earlier)
                    })
                {
                    continue;
                }
                if replays == MAX_REPLAYS {
                    return Ok(best);
                }
                replays += 1;
                let candidate = prefix.append(base, plans, index).await?;
                // Conflict costs can only grow along a prefix.
                if candidate.conflicts < best.conflicts {
                    if candidate.order.len() == plans.len() {
                        best = candidate.clone();
                        if best.conflicts == 0 {
                            return Ok(best);
                        }
                    }
                    next.push(candidate);
                }
            }
        }
        next.sort_by(|a, b| (a.conflicts, &a.order).cmp(&(b.conflicts, &b.order)));
        next.truncate(BEAM_WIDTH);
        if next.is_empty() {
            break;
        }
        beam = next;
    }
    Ok(best)
}

/// The paths that differ between two trees, named the way a report shows them.
async fn changed_file_names(
    before: &MergedTree,
    after: &MergedTree,
) -> Result<Vec<String>, String> {
    Ok(changed_paths(before, after)
        .await?
        .iter()
        .map(|path| path.as_internal_file_string().to_owned())
        .collect())
}

async fn changed_paths(
    before: &MergedTree,
    after: &MergedTree,
) -> Result<BTreeSet<RepoPathBuf>, String> {
    use futures::StreamExt as _;

    let mut stream = before.diff_stream(after, &EverythingMatcher);
    let mut paths = BTreeSet::new();
    while let Some(entry) = stream.next().await {
        entry.values.map_err(|e| {
            format!(
                "could not read tree difference at {}: {e}",
                entry.path.as_internal_file_string()
            )
        })?;
        paths.insert(entry.path);
    }
    Ok(paths)
}

async fn paths_from(
    base: &MergedTree,
    source: &MergedTree,
    paths: &BTreeSet<RepoPathBuf>,
) -> Result<MergedTree, String> {
    let mut builder = MergedTreeBuilder::new(base.clone());
    for path in paths {
        let value = source.path_value(path).await.map_err(|e| {
            format!(
                "could not read causal-context path {}: {e}",
                path.as_internal_file_string()
            )
        })?;
        builder.set_or_remove(path.clone(), value);
    }
    builder
        .write_tree()
        .await
        .map_err(|e| format!("could not write causal-context tree: {e}"))
}

async fn apply_delta(
    destination: &MergedTree,
    before: &MergedTree,
    after: &MergedTree,
) -> jj_lib::backend::BackendResult<MergedTree> {
    MergedTree::merge(Merge::from_vec(vec![
        (destination.clone(), "extraction destination".to_string()),
        (before.clone(), "before recorded edit".to_string()),
        (after.clone(), "after recorded edit".to_string()),
    ]))
    .await
}

fn load_commit(store: &Arc<Store>, hex: &str) -> Result<Commit, String> {
    let id = CommitId::try_from_hex(hex)
        .ok_or_else(|| format!("recorded commit id is not valid hex: {hex}"))?;
    store
        .get_commit(&id)
        .map_err(|e| format!("could not load recorded commit {hex}: {e}"))
}

fn extraction_settings(
    operation_username: &str,
    user_name: &str,
    user_email: &str,
) -> Result<UserSettings, String> {
    let mut layer = ConfigLayer::empty(ConfigSource::User);
    layer
        .set_value("user.name", user_name)
        .and_then(|_| layer.set_value("user.email", user_email))
        .and_then(|_| layer.set_value("operation.username", operation_username))
        .and_then(|_| layer.set_value("operation.hostname", "jj-extract.local"))
        .and_then(|_| layer.set_value("merge.hunk-level", "word"))
        .map_err(|e| format!("could not build jj-lib settings: {e}"))?;
    let mut config = StackedConfig::with_defaults();
    config.add_layer(layer);
    UserSettings::from_config(config).map_err(|e| format!("could not load jj-lib settings: {e}"))
}

fn short_hex(hex: String) -> String {
    hex.chars().take(12).collect()
}

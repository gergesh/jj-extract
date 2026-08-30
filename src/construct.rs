//! Extract recorded agent edits in one jj transaction.
//!
//! Every attributed evolution contributes the tree delta from its predecessor
//! to itself. Neutral snapshots preceding an edit are carried on the paths that
//! edit touched, then removed again when their inverse commutes cleanly. We
//! compose those deltas in memory, rewrite the extracted stack and the live
//! working-copy commit once, then publish one operation. This keeps the operation
//! log as the attribution ledger while making one `jj undo` reverse a complete
//! extraction.

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
use jj_lib::settings::UserSettings;
use jj_lib::store::Store;
use jj_lib::workspace::Workspace;

use crate::identity;
use crate::jj::{Evolution, Jj};

pub struct Built {
    pub session: String,
    pub change_id: String,
    pub conflict: bool,
    /// True when this updated a prior extraction of the same session.
    pub updated: bool,
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

/// Session -> the changes extracted for it, newest last. A session can hold
/// more than one: extracting the same session from a different branch line
/// builds a change of its own there.
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
/// operations. Absent (or unreadable, which can only mean a version wrote a
/// shape this one doesn't know) it starts empty: extraction then builds a fresh
/// change instead of updating one, which is the same outcome as a first run.
async fn read_ledger(repo: &ReadonlyRepo) -> Result<Ledger, String> {
    use futures::TryStreamExt as _;

    let head: Operation = repo.operation().clone();
    let mut operations = Box::pin(op_walk::walk_ancestors(&[head]));
    while let Some(operation) = operations
        .try_next()
        .await
        .map_err(|e| format!("could not read the operation log: {e}"))?
    {
        if let Some(raw) = operation.metadata().attributes.get(LEDGER_ATTRIBUTE) {
            return Ok(serde_json::from_str(raw).unwrap_or_default());
        }
    }
    Ok(Ledger::new())
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
) -> Result<Vec<Built>, String> {
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
    ))
}

async fn extract_async(
    root: &Path,
    evolog: &[Evolution],
    targets: &[String],
    sessions: &[String],
    user_name: &str,
    user_email: &str,
) -> Result<Vec<Built>, String> {
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

    let oldest = load_commit(store, &evolog[0].commit)?;
    let base_id = oldest
        .parent_ids()
        .first()
        .cloned()
        .ok_or_else(|| "the oldest recorded evolution has no parent".to_string())?;
    let base = store
        .get_commit(&base_id)
        .map_err(|e| format!("could not load the extraction base: {e}"))?;

    let mut ledger = read_ledger(repo.as_ref()).await?;
    let mut existing = existing_extractions(repo.as_ref(), &base, sessions, &ledger).await?;

    let mut target_trees = HashMap::new();
    for session in targets {
        if let Some(tree) = compose_session_tree(store, &base, session, evolog).await? {
            target_trees.insert(session.clone(), tree);
        }
    }
    if target_trees.is_empty() {
        return Ok(Vec::new());
    }

    let old_live_tree = live.tree();
    let mut tx = repo.start_transaction();
    tx.set_workspace_name(&workspace_name);
    let mut tip = base.clone();
    let mut built = Vec::new();

    for session in sessions {
        let mut prior_commits = existing.remove(session).unwrap_or_default();
        let prior = prior_commits.first().cloned();
        let (delta_base, delta_tree, description, is_target) =
            if let Some(tree) = target_trees.get(session) {
                (
                    base.tree(),
                    tree.clone(),
                    // A change that already has a description keeps it:
                    // re-extraction updates what the change contains, never the
                    // words its author chose for it.
                    prior
                        .as_ref()
                        .map(|commit| commit.description().to_owned())
                        .filter(|description| !description.is_empty())
                        .unwrap_or_else(|| {
                            extraction_desc(session, kind_of(evolog, session).as_deref())
                        }),
                    true,
                )
            } else if let Some(commit) = &prior {
                (
                    commit.parent_tree(repo.as_ref()).await.map_err(|e| {
                        format!("could not read {session}'s prior parent tree: {e}")
                    })?,
                    commit.tree(),
                    commit.description().to_owned(),
                    false,
                )
            } else {
                continue;
            };

        let stacked_tree = apply_delta(&tip.tree(), &delta_base, &delta_tree)
            .await
            .map_err(|e| format!("could not stack session {session}: {e}"))?;
        let updated = prior.is_some();
        let commit = if let Some(prior) = prior {
            tx.repo_mut()
                .rewrite_commit(&prior)
                .set_parents(vec![tip.id().clone()])
                .set_tree(stacked_tree)
                .set_description(description)
                .write()
                .await
        } else {
            tx.repo_mut()
                .new_commit(vec![tip.id().clone()], stacked_tree)
                .set_description(description)
                .write()
                .await
        }
        .map_err(|e| format!("could not write extracted session {session}: {e}"))?;

        if prior_commits.len() > 1 {
            for duplicate in prior_commits.drain(1..) {
                tx.repo_mut().record_abandoned_commit(&duplicate);
            }
        }
        if is_target {
            let extracted = ledger.entry(session.clone()).or_default();
            let change = commit.change_id().hex();
            if !extracted.contains(&change) {
                extracted.push(change);
            }
            built.push(Built {
                session: session.clone(),
                change_id: short_hex(commit.change_id().reverse_hex()),
                conflict: commit.has_conflict(),
                updated,
            });
        }
        tip = commit;
    }

    let new_live = tx
        .repo_mut()
        .rewrite_commit(&live)
        .set_parents(vec![tip.id().clone()])
        .set_tree(old_live_tree.clone())
        .write()
        .await
        .map_err(|e| {
            format!("could not place the live working copy on the extraction stack: {e}")
        })?;
    tx.repo_mut()
        .set_wc_commit(workspace_name.clone(), new_live.id().clone())
        .map_err(|e| format!("could not preserve the live working-copy change: {e}"))?;
    tx.repo_mut()
        .rebase_descendants()
        .await
        .map_err(|e| format!("could not rebase descendants of extracted changes: {e}"))?;

    tx.set_attribute(
        LEDGER_ATTRIBUTE.to_string(),
        serde_json::to_string(&ledger)
            .map_err(|e| format!("could not write the extraction ledger: {e}"))?,
    );
    let operation_description = if built.len() == 1 {
        format!("extract session {}", built[0].session)
    } else {
        format!("extract {} sessions", built.len())
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
    workspace
        .check_out(new_repo.op_id().clone(), Some(&old_live_tree), &checked_out)
        .await
        .map_err(|e| format!("could not update the working copy after extraction: {e}"))?;

    Ok(built)
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

async fn compose_session_tree(
    store: &Arc<Store>,
    base: &Commit,
    session: &str,
    evolog: &[Evolution],
) -> Result<Option<MergedTree>, String> {
    let mut result = base.tree();
    let neutral = evolog.first().map(|e| e.user.as_str()).unwrap_or("");
    let mut carried_neutral = Vec::new();
    let mut found = false;
    for index in 1..evolog.len() {
        if evolog[index].user != session {
            continue;
        }
        let before = load_commit(store, &evolog[index - 1].commit)?;
        let after = load_commit(store, &evolog[index].commit)?;

        // A neutral snapshot between two edits from the same session can be
        // causal context for the later edit (most commonly formatter output). Add
        // only overlapping paths before replaying the attributed delta. We try
        // to remove that context again after all attributed edits have been
        // composed; if the inverse conflicts, the session depended on it and
        // the context belongs with the extracted change.
        let mut context_start = index - 1;
        while context_start > 0
            && evolog[context_start].user == neutral
            && evolog[context_start].is_snapshot
        {
            context_start -= 1;
        }
        if context_start < index - 1 {
            let context_before = load_commit(store, &evolog[context_start].commit)?;
            let actual_paths = changed_paths(&before.tree(), &after.tree()).await?;
            let neutral_paths = changed_paths(&context_before.tree(), &before.tree()).await?;
            let overlapping_paths = neutral_paths
                .intersection(&actual_paths)
                .cloned()
                .collect::<BTreeSet<_>>();
            if !overlapping_paths.is_empty() {
                let restricted_before =
                    paths_from(&base.tree(), &context_before.tree(), &overlapping_paths).await?;
                let restricted_after =
                    paths_from(&base.tree(), &before.tree(), &overlapping_paths).await?;
                result = apply_delta(&result, &restricted_before, &restricted_after)
                    .await
                    .map_err(|e| format!("could not replay causal context for {session}: {e}"))?;
                carried_neutral.push((restricted_before, restricted_after));
            }
        }
        result = apply_delta(&result, &before.tree(), &after.tree())
            .await
            .map_err(|e| format!("could not compose a recorded edit for {session}: {e}"))?;
        found = true;
    }

    for (before, after) in carried_neutral.into_iter().rev() {
        let candidate = apply_delta(&result, &after, &before)
            .await
            .map_err(|e| format!("could not remove neutral context for {session}: {e}"))?;
        if !candidate.has_conflict() {
            result = candidate;
        }
    }
    Ok(found.then_some(result))
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
        .map_err(|e| format!("could not build jj-lib settings: {e}"))?;
    let mut config = StackedConfig::with_defaults();
    config.add_layer(layer);
    UserSettings::from_config(config).map_err(|e| format!("could not load jj-lib settings: {e}"))
}

fn short_hex(hex: String) -> String {
    hex.chars().take(12).collect()
}

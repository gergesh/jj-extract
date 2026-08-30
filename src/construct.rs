//! Extract recorded agent edits in one jj transaction.
//!
//! Every attributed evolution contributes the tree delta from its predecessor
//! to itself. We compose those deltas in memory, rewrite the extracted stack and
//! the live working-copy commit once, then publish one operation. This keeps the
//! operation log as the attribution ledger while making one `jj undo` reverse a
//! complete extraction.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::default_backend_factories::{
    default_backend_factories, default_working_copy_factories,
};
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::repo::Repo as _;
use jj_lib::settings::UserSettings;
use jj_lib::store::Store;
use jj_lib::workspace::Workspace;

use crate::jj::{Evolution, Jj};

pub struct Built {
    pub session: String,
    pub change_id: String,
    pub conflict: bool,
    /// True when this updated a prior extraction of the same session.
    pub updated: bool,
}

/// A stable, machine-readable trailer identifying the owning session.
pub fn session_trailer(session: &str) -> String {
    format!("jj-extract-session: {session}")
}

fn extraction_desc(session: &str, message: Option<&str>) -> String {
    let body = message
        .map(str::to_owned)
        .unwrap_or_else(|| format!("jj-extract: {session}"));
    format!("{body}\n\n{}", session_trailer(session))
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
    targets: &[(String, Option<String>)],
) -> Result<Vec<Built>, String> {
    let sessions = agents_in(evolog);
    let mut existing_ids = HashMap::new();
    for session in &sessions {
        existing_ids.insert(
            session.clone(),
            jj.commits_with_description(&session_trailer(session))?,
        );
    }
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
        existing_ids,
        settings.user_name(),
        settings.user_email(),
    ))
}

async fn extract_async(
    root: &Path,
    evolog: &[Evolution],
    targets: &[(String, Option<String>)],
    sessions: &[String],
    existing_ids: HashMap<String, Vec<String>>,
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

    let mut existing: HashMap<String, Vec<Commit>> = HashMap::new();
    for (session, ids) in existing_ids {
        let commits = ids
            .iter()
            .map(|id| load_commit(store, id))
            .collect::<Result<Vec<_>, _>>()?;
        existing.insert(session, commits);
    }

    let target_messages: HashMap<&str, Option<&str>> = targets
        .iter()
        .map(|(session, message)| (session.as_str(), message.as_deref()))
        .collect();
    let mut target_trees = HashMap::new();
    for session in target_messages.keys() {
        if let Some(tree) = compose_session_tree(store, &base, session, evolog).await? {
            target_trees.insert((*session).to_string(), tree);
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
                    extraction_desc(session, target_messages[session.as_str()]),
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

async fn compose_session_tree(
    store: &Arc<Store>,
    base: &Commit,
    session: &str,
    evolog: &[Evolution],
) -> Result<Option<MergedTree>, String> {
    let mut result = base.tree();
    let mut found = false;
    for index in 1..evolog.len() {
        if evolog[index].user != session {
            continue;
        }
        let before = load_commit(store, &evolog[index - 1].commit)?;
        let after = load_commit(store, &evolog[index].commit)?;
        result = apply_delta(&result, &before.tree(), &after.tree())
            .await
            .map_err(|e| format!("could not compose a recorded edit for {session}: {e}"))?;
        found = true;
    }
    Ok(found.then_some(result))
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

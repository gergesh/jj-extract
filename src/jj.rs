//! Direct `jj-lib` access for recording and reading attributed edits.

use std::path::{Path, PathBuf};

use futures::TryStreamExt as _;
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::default_backend_factories::{
    default_backend_factories, default_working_copy_factories,
};
use jj_lib::evolution::walk_predecessors;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{FilesMatcher, NothingMatcher};
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::settings::{HumanByteSize, UserSettings};
use jj_lib::working_copy::SnapshotOptions;
use jj_lib::workspace::Workspace;

use crate::jj_config::config_home;

pub struct Jj {
    root: PathBuf,
}

/// Attribute naming the agent product that recorded a snapshot, so extraction
/// can write that product's own co-author trailer.
const KIND_ATTRIBUTE: &str = "jj-extract.agent-kind";

/// One evolution of `@`: its commit id and the username on the operation that
/// created it (i.e. the agent we tagged, or a neutral default).
pub struct Evolution {
    pub commit: String,
    pub user: String,
    /// Which agent product recorded it, when the hook knew.
    pub kind: Option<String>,
    /// Whether jj marked this evolution as a pure working-copy snapshot.
    pub is_snapshot: bool,
}

impl Jj {
    pub fn new(root: &Path) -> Jj {
        Jj {
            root: root.to_path_buf(),
        }
    }

    pub fn settings(&self, operation_username: &str) -> Result<UserSettings, String> {
        settings(&self.root, Some(operation_username))
    }

    /// Snapshot the working copy under a neutral operation so edits already on
    /// disk cannot be attributed to the next agent.
    pub fn snapshot_neutral(&self) {
        let _ = pollster::block_on(self.snapshot(None, None, &[]));
    }

    /// Snapshot the working copy in an operation owned by `agent`. Every tracked
    /// path is snapshotted by jj's working-copy implementation; the named paths
    /// (the files this edit created) are the only ones allowed to *start* being
    /// tracked, so editing an untracked file never pulls it into the repo.
    pub fn snapshot_tagged(&self, agent: &str, kind: Option<&str>, paths: &[String]) {
        let _ = pollster::block_on(self.snapshot(Some(agent), kind, paths));
    }

    async fn snapshot(
        &self,
        operation_username: Option<&str>,
        kind: Option<&str>,
        paths: &[String],
    ) -> Result<(), String> {
        let settings = settings(&self.root, operation_username)?;
        let mut workspace = self.load_workspace(&settings)?;
        let repo = workspace
            .repo_loader()
            .load_at_head()
            .await
            .map_err(|e| format!("could not load the jj repository: {e}"))?;
        let workspace_name = workspace.workspace_name().to_owned();
        let wc_id = repo
            .view()
            .get_wc_commit_id(&workspace_name)
            .cloned()
            .ok_or_else(|| "could not resolve the working-copy commit".to_string())?;
        let wc_commit = repo
            .store()
            .get_commit(&wc_id)
            .map_err(|e| format!("could not load the working-copy commit: {e}"))?;

        let repo_paths = paths
            .iter()
            .filter(|path| self.root.join(path).exists())
            .map(|path| RepoPathBuf::from_internal_string(path.clone()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("invalid repository path in hook payload: {e}"))?;
        let files = FilesMatcher::new(&repo_paths);
        let nothing = NothingMatcher;
        let start_tracking = if repo_paths.is_empty() {
            &nothing as &dyn jj_lib::matchers::Matcher
        } else {
            &files as &dyn jj_lib::matchers::Matcher
        };
        let options = SnapshotOptions {
            base_ignores: GitIgnoreFile::empty(),
            progress: None,
            start_tracking_matcher: start_tracking,
            force_tracking_matcher: &nothing,
            max_new_file_size: max_new_file_size(&settings),
        };

        let mut locked = workspace
            .start_working_copy_mutation()
            .await
            .map_err(|e| format!("could not lock the jj working copy: {e}"))?;
        if locked.locked_wc().old_operation_id() != repo.op_id() {
            return Err("the jj working copy changed while preparing its snapshot".to_string());
        }
        let (new_tree, _stats) = locked
            .locked_wc()
            .snapshot(&options)
            .await
            .map_err(|e| format!("could not snapshot the jj working copy: {e}"))?;
        if new_tree.tree_ids_and_labels() == wc_commit.tree().tree_ids_and_labels() {
            locked
                .finish(repo.op_id().clone())
                .await
                .map_err(|e| format!("could not save the jj working-copy state: {e}"))?;
            return Ok(());
        }

        let mut tx = repo.start_transaction();
        tx.set_is_snapshot(true);
        tx.set_workspace_name(&workspace_name);
        if let Some(kind) = kind {
            tx.set_attribute(KIND_ATTRIBUTE.to_string(), kind.to_string());
        }
        let new_wc = tx
            .repo_mut()
            .rewrite_commit(&wc_commit)
            .set_tree(new_tree)
            .write()
            .await
            .map_err(|e| format!("could not write the working-copy snapshot: {e}"))?;
        tx.repo_mut()
            .set_wc_commit(workspace_name, new_wc.id().clone())
            .map_err(|e| format!("could not update the working-copy commit: {e}"))?;
        tx.repo_mut()
            .rebase_descendants()
            .await
            .map_err(|e| format!("could not rebase after the working-copy snapshot: {e}"))?;
        let new_repo = tx
            .commit("snapshot working copy")
            .await
            .map_err(|e| format!("could not publish the working-copy snapshot: {e}"))?;
        locked
            .finish(new_repo.op_id().clone())
            .await
            .map_err(|e| format!("could not save the jj working-copy state: {e}"))?;
        Ok(())
    }

    /// `@`'s evolutions, oldest first, each with the tagging operation's user.
    pub fn evolog(&self) -> Result<Vec<Evolution>, String> {
        pollster::block_on(self.evolog_async())
    }

    async fn evolog_async(&self) -> Result<Vec<Evolution>, String> {
        let neutral = "jj-extract";
        let settings = settings(&self.root, None)?;
        let workspace = self.load_workspace(&settings)?;
        let repo = workspace
            .repo_loader()
            .load_at_head()
            .await
            .map_err(|e| format!("could not load the jj repository: {e}"))?;
        let wc_id = repo
            .view()
            .get_wc_commit_id(workspace.workspace_name())
            .cloned()
            .ok_or_else(|| "could not resolve the working-copy commit".to_string())?;
        let mut entries: Vec<_> = walk_predecessors(&repo, &[wc_id])
            .try_collect()
            .await
            .map_err(|e| format!("could not read the working-copy evolution log: {e}"))?;
        let mut evolutions = entries
            .drain(..)
            .map(|entry| Evolution {
                commit: entry.commit.id().hex(),
                user: entry
                    .operation
                    .as_ref()
                    .map(|op| op.metadata().username.clone())
                    .filter(|user| !user.is_empty())
                    .unwrap_or_else(|| neutral.to_string()),
                kind: entry
                    .operation
                    .as_ref()
                    .and_then(|op| op.metadata().attributes.get(KIND_ATTRIBUTE))
                    .cloned(),
                is_snapshot: entry
                    .operation
                    .as_ref()
                    .is_some_and(|op| op.metadata().is_snapshot),
            })
            .collect::<Vec<_>>();
        evolutions.reverse();
        Ok(evolutions)
    }

    fn load_workspace(&self, settings: &UserSettings) -> Result<Workspace, String> {
        Workspace::load(
            settings,
            &self.root,
            &default_backend_factories(),
            &default_working_copy_factories(),
        )
        .map_err(|e| format!("could not load the jj workspace: {e}"))
    }
}

fn settings(root: &Path, operation_username: Option<&str>) -> Result<UserSettings, String> {
    let neutral_username = neutral_username();
    let mut layer = ConfigLayer::empty(ConfigSource::User);
    layer
        .set_value("user.name", "jj-extract")
        .and_then(|_| layer.set_value("user.email", "jj-extract@localhost"))
        .and_then(|_| layer.set_value("operation.username", neutral_username))
        .and_then(|_| layer.set_value("operation.hostname", "jj-extract.local"))
        .map_err(|e| format!("could not build jj-lib settings: {e}"))?;
    let mut config = StackedConfig::with_defaults();
    config.add_layer(layer);
    load_config_files(&mut config, root)?;
    if let Some(operation_username) = operation_username {
        let mut overrides = ConfigLayer::empty(ConfigSource::CommandArg);
        overrides
            .set_value("operation.username", operation_username)
            .map_err(|e| format!("could not build jj-lib operation settings: {e}"))?;
        config.add_layer(overrides);
    }
    UserSettings::from_config(config).map_err(|e| format!("could not load jj-lib settings: {e}"))
}

fn load_config_files(config: &mut StackedConfig, root: &Path) -> Result<(), String> {
    if let Some(paths) = std::env::var_os("JJ_CONFIG") {
        for path in std::env::split_paths(&paths).filter(|path| !path.as_os_str().is_empty()) {
            load_config_path(config, ConfigSource::User, &path)?;
        }
    } else {
        load_config_path(
            config,
            ConfigSource::System,
            Path::new("/etc/jj/config.toml"),
        )?;
        load_config_path(config, ConfigSource::System, Path::new("/etc/jj/conf.d"))?;
        if let Some(home) = dirs::home_dir() {
            load_config_path(config, ConfigSource::User, &home.join(".jjconfig.toml"))?;
        }
        if let Some(config_home) = config_home() {
            load_config_path(
                config,
                ConfigSource::User,
                &config_home.join("jj/config.toml"),
            )?;
            load_config_path(config, ConfigSource::User, &config_home.join("jj/conf.d"))?;
        }
    }

    let workspace_dir = root.join(".jj");
    let repo_dir = resolve_repo_dir(&workspace_dir)?;
    load_config_path(config, ConfigSource::Repo, &repo_dir.join("config.toml"))?;
    if let Some(path) = secure_config_path(&repo_dir, "config-id", "repos")? {
        load_config_path(config, ConfigSource::Repo, &path)?;
    }
    if let Some(path) = secure_config_path(&workspace_dir, "workspace-config-id", "workspaces")? {
        load_config_path(config, ConfigSource::Workspace, &path)?;
    }
    Ok(())
}

fn load_config_path(
    config: &mut StackedConfig,
    source: ConfigSource,
    path: &Path,
) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let result = if path.is_dir() {
        config.load_dir(source, path)
    } else {
        config.load_file(source, path)
    };
    result.map_err(|e| format!("could not load jj config {}: {e}", path.display()))
}

fn resolve_repo_dir(workspace_dir: &Path) -> Result<PathBuf, String> {
    let locator = workspace_dir.join("repo");
    if locator.is_dir() {
        return Ok(locator);
    }
    let value = std::fs::read_to_string(&locator).map_err(|e| {
        format!(
            "could not read jj repository locator {}: {e}",
            locator.display()
        )
    })?;
    let path = PathBuf::from(value.trim());
    Ok(if path.is_absolute() {
        path
    } else {
        workspace_dir.join(path)
    })
}

fn secure_config_path(
    state_dir: &Path,
    id_file: &str,
    category: &str,
) -> Result<Option<PathBuf>, String> {
    let id_path = state_dir.join(id_file);
    let id = match std::fs::read_to_string(&id_path) {
        Ok(id) => id,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "could not read jj config id {}: {error}",
                id_path.display()
            ));
        }
    };
    Ok(config_home().map(|home| {
        home.join("jj")
            .join(category)
            .join(id.trim())
            .join("config.toml")
    }))
}

fn max_new_file_size(settings: &UserSettings) -> u64 {
    let HumanByteSize(size) = settings
        .get_value_with("snapshot.max-new-file-size", TryInto::try_into)
        .unwrap_or(HumanByteSize(1024 * 1024));
    if size == 0 {
        u64::MAX
    } else {
        size
    }
}

fn neutral_username() -> String {
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    if username.is_empty() {
        "jj-extract".to_string()
    } else {
        username
    }
}

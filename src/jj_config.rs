//! Management of jj-extract's owned jj user-config fragment.

use std::path::{Path, PathBuf};

use jj_lib::config::{ConfigFile, ConfigSource, ConfigValue};

const FRAGMENT_NAME: &str = "zz-jj-extract.toml";
const ALIAS_KEY: &str = "aliases.extract";

/// The platform user-config directory; jj uses its `jj/` subdirectory.
pub fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
}

/// Install `jj extract` as an external-subcommand alias in an owned fragment.
pub fn install_alias(executable: String) -> Result<PathBuf, String> {
    let path = fragment_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("jj config fragment has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|e| {
        format!(
            "could not create jj config directory {}: {e}",
            parent.display()
        )
    })?;

    let mut file = ConfigFile::load_or_empty(ConfigSource::User, path.clone())
        .map_err(|e| format!("could not load jj config fragment {}: {e}", path.display()))?;
    file.set_value(
        ALIAS_KEY,
        ConfigValue::from_iter(["util", "exec", "--", executable.as_str()]),
    )
    .map_err(|e| format!("could not set {ALIAS_KEY}: {e}"))?;
    file.save()
        .map_err(|e| format!("could not save jj config fragment {}: {e}", path.display()))?;
    Ok(path)
}

/// Remove only an alias that still points at jj-extract.
pub fn uninstall_alias() -> Result<bool, String> {
    let path = fragment_path()?;
    if !path.exists() {
        return Ok(false);
    }
    let mut file = ConfigFile::load_or_empty(ConfigSource::User, path.clone())
        .map_err(|e| format!("could not load jj config fragment {}: {e}", path.display()))?;
    let Some(alias) = file
        .layer()
        .look_up_item(ALIAS_KEY)
        .map_err(|_| format!("{ALIAS_KEY} is nested below a non-table value"))?
    else {
        return Ok(false);
    };
    if !is_owned_alias(alias.as_value()) {
        return Err(format!(
            "refusing to remove {ALIAS_KEY} because it is not a jj-extract alias"
        ));
    }
    file.delete_value(ALIAS_KEY)
        .map_err(|e| format!("could not remove {ALIAS_KEY}: {e}"))?;
    file.save()
        .map_err(|e| format!("could not save jj config fragment {}: {e}", path.display()))?;
    Ok(true)
}

fn fragment_path() -> Result<PathBuf, String> {
    config_home()
        .map(|home| home.join("jj/conf.d").join(FRAGMENT_NAME))
        .ok_or_else(|| "could not determine the user config directory".to_string())
}

fn is_owned_alias(value: Option<&ConfigValue>) -> bool {
    value
        .and_then(ConfigValue::as_array)
        .and_then(|parts| parts.iter().last())
        .and_then(ConfigValue::as_str)
        .and_then(|part| Path::new(part).file_name())
        .and_then(|name| name.to_str())
        == Some("jj-extract")
}

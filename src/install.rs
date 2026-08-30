//! Register / unregister jj-extract hooks in Claude Code and Codex JSON config,
//! preserving every other key and any pre-existing hooks.

use serde_json::{json, Value};
use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};

/// Claude needs SessionStart to stamp its identity plus Pre/PostToolUse around
/// its file tools. Codex already exports its thread identity, so it only needs
/// Pre/PostToolUse around apply_patch. Each PreToolUse parks pre-existing content
/// so PostToolUse collects only that tool use's delta.
const CLAUDE_REGISTRATIONS: [(&str, Option<&str>); 3] = [
    ("SessionStart", None),
    ("PreToolUse", Some("Edit|Write|MultiEdit|Bash")),
    ("PostToolUse", Some("Edit|Write|MultiEdit|Bash")),
];
const CODEX_REGISTRATIONS: [(&str, Option<&str>); 2] = [
    ("PreToolUse", Some("^apply_patch$")),
    ("PostToolUse", Some("^apply_patch$")),
];

fn is_our_hook(command: &str) -> bool {
    command.contains("jj-extract") && command.contains("hook")
}

/// Validate an existing hook config without modifying it.
pub fn validate_settings_file(path: &Path) -> std::io::Result<()> {
    validate_settings(&read_settings(path)?)
}

/// Idempotently add the Claude Code hooks. Returns true if anything changed.
pub fn merge_claude_settings(path: &Path, command: &str) -> std::io::Result<bool> {
    merge_settings(path, command, &CLAUDE_REGISTRATIONS)
}

/// Idempotently add the Codex hooks. Returns true if anything changed.
pub fn merge_codex_settings(path: &Path, command: &str) -> std::io::Result<bool> {
    merge_settings(path, command, &CODEX_REGISTRATIONS)
}

fn merge_settings(
    path: &Path,
    command: &str,
    registrations: &[(&str, Option<&str>)],
) -> std::io::Result<bool> {
    let mut data = read_settings(path)?;
    validate_settings(&data)?;
    let hooks = data
        .as_object_mut()
        .expect("validated settings object")
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let mut changed = false;

    for &(event, matcher) in registrations {
        let groups = hooks
            .as_object_mut()
            .expect("validated hooks object")
            .entry(event)
            .or_insert_with(|| json!([]));
        let arr = groups.as_array_mut().expect("validated hook groups");
        // Find a group with the same matcher.
        let existing = arr.iter_mut().find(|grp| group_matches(grp, matcher));
        if let Some(grp) = existing {
            let entry = grp
                .as_object_mut()
                .expect("validated hook group")
                .entry("hooks")
                .or_insert_with(|| json!([]));
            let list = entry.as_array_mut().expect("validated hook entries");
            let present = list.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .map(is_our_hook)
                    .unwrap_or(false)
            });
            if !present {
                list.push(json!({"type": "command", "command": command}));
                changed = true;
            }
        } else {
            let mut grp = serde_json::Map::new();
            if let Some(m) = matcher {
                grp.insert("matcher".into(), json!(m));
            }
            grp.insert(
                "hooks".into(),
                json!([{"type": "command", "command": command}]),
            );
            arr.push(Value::Object(grp));
            changed = true;
        }
    }

    if changed {
        write_with_backup(path, &data)?;
    }
    Ok(changed)
}

/// Remove our hooks, leaving other hooks intact. Returns how many were removed.
pub fn remove_settings(path: &Path) -> std::io::Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let mut data = read_settings(path)?;
    validate_settings(&data)?;
    let Some(hooks) = data.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(0);
    };
    let mut removed = 0usize;
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(groups) = hooks.get_mut(&event).and_then(Value::as_array_mut) else {
            continue;
        };
        for grp in groups.iter_mut() {
            if let Some(list) = grp.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = list.len();
                list.retain(|h| {
                    !h.get("command")
                        .and_then(Value::as_str)
                        .map(is_our_hook)
                        .unwrap_or(false)
                });
                removed += before - list.len();
            }
        }
        groups.retain(|grp| {
            grp.get("hooks")
                .and_then(Value::as_array)
                .map(|l| !l.is_empty())
                .unwrap_or(false)
        });
    }
    // Drop now-empty event arrays.
    let empty: Vec<String> = hooks
        .iter()
        .filter(|(_, v)| v.as_array().map(|a| a.is_empty()).unwrap_or(false))
        .map(|(k, _)| k.clone())
        .collect();
    for k in empty {
        hooks.remove(&k);
    }

    if removed > 0 {
        write_with_backup(path, &data)?;
    }
    Ok(removed)
}

fn group_matches(grp: &Value, matcher: Option<&str>) -> bool {
    let existing = grp.get("matcher").and_then(Value::as_str);
    match matcher {
        Some(m) => existing == Some(m),
        None => existing.is_none(),
    }
}

fn read_settings(path: &Path) -> std::io::Result<Value> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(json!({})),
        Err(e) => return Err(e),
    };
    serde_json::from_slice(&bytes).map_err(|e| {
        invalid_data(format!(
            "{} is not valid JSON ({e}); fix it before installing jj-extract",
            path.display()
        ))
    })
}

fn validate_settings(data: &Value) -> std::io::Result<()> {
    let root = data
        .as_object()
        .ok_or_else(|| invalid_data("hook config must be a JSON object"))?;
    let Some(hooks) = root.get("hooks") else {
        return Ok(());
    };
    let hooks = hooks
        .as_object()
        .ok_or_else(|| invalid_data("hook config `hooks` must be a JSON object"))?;
    for (event, groups) in hooks {
        let groups = groups
            .as_array()
            .ok_or_else(|| invalid_data(format!("hook event `{event}` must be an array")))?;
        for (index, group) in groups.iter().enumerate() {
            let group = group.as_object().ok_or_else(|| {
                invalid_data(format!("hook group `{event}[{index}]` must be an object"))
            })?;
            if let Some(matcher) = group.get("matcher") {
                if !matcher.is_string() {
                    return Err(invalid_data(format!(
                        "hook matcher `{event}[{index}].matcher` must be a string"
                    )));
                }
            }
            if let Some(entries) = group.get("hooks") {
                if !entries.is_array() {
                    return Err(invalid_data(format!(
                        "hook entries `{event}[{index}].hooks` must be an array"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn invalid_data(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidData, message.into())
}

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut sibling = path.as_os_str().to_owned();
    sibling.push(suffix);
    PathBuf::from(sibling)
}

fn write_with_backup(path: &Path, data: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let orig = std::fs::read(path)?;
        std::fs::write(sibling_path(path, ".jj-extract-bak"), orig)?;
    }
    let mut text = serde_json::to_string_pretty(data)?;
    text.push('\n');
    let temporary = sibling_path(path, &format!(".jj-extract-tmp-{}", std::process::id()));
    std::fs::write(&temporary, text)?;
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(temporary);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{merge_claude_settings, merge_codex_settings, remove_settings, sibling_path};
    use serde_json::Value;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn test_dir() -> PathBuf {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "jj-extract-install-test-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn install_is_idempotent_and_preserves_other_settings() {
        let dir = test_dir();
        let path = dir.join("settings.json");
        let original = r#"{
  "permissions": {"allow": ["Read"]},
  "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "check"}]}]}
}"#;
        fs::write(&path, original).unwrap();

        assert!(merge_claude_settings(&path, "jj-extract --hook").unwrap());
        assert!(!merge_claude_settings(&path, "jj-extract --hook").unwrap());

        let installed: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(installed["permissions"]["allow"][0], "Read");
        assert_eq!(
            installed["hooks"]["PreToolUse"].as_array().unwrap().len(),
            2
        );
        assert_eq!(
            fs::read(sibling_path(&path, ".jj-extract-bak")).unwrap(),
            original.as_bytes()
        );

        assert_eq!(remove_settings(&path).unwrap(), 3);
        let removed: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(removed["permissions"]["allow"][0], "Read");
        assert_eq!(removed["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_json_is_never_replaced() {
        let dir = test_dir();
        let path = dir.join("settings.json");
        let original = b"{ not json";
        fs::write(&path, original).unwrap();

        let error = merge_claude_settings(&path, "jj-extract --hook").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!sibling_path(&path, ".jj-extract-bak").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn malformed_hook_shapes_return_errors_instead_of_panicking() {
        let dir = test_dir();
        let path = dir.join("settings.json");
        let original = br#"{"hooks":"unexpected"}"#;
        fs::write(&path, original).unwrap();

        assert!(merge_claude_settings(&path, "jj-extract --hook").is_err());
        assert!(remove_settings(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn codex_install_uses_the_native_apply_patch_matcher() {
        let dir = test_dir();
        let path = dir.join("hooks.json");

        assert!(merge_codex_settings(&path, "jj-extract --hook").unwrap());
        let installed: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(installed["hooks"].get("SessionStart").is_none());
        for event in ["PreToolUse", "PostToolUse"] {
            assert_eq!(installed["hooks"][event][0]["matcher"], "^apply_patch$");
        }
        assert_eq!(remove_settings(&path).unwrap(), 2);
        fs::remove_dir_all(dir).unwrap();
    }
}

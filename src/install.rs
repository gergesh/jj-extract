//! Register / unregister the jj-collect hooks in Claude Code settings.json,
//! preserving every other key and any pre-existing hooks.

use serde_json::{json, Value};
use std::path::Path;

/// The three hook registrations jj-collect needs:
/// SessionStart (stamp identity), and Pre/PostToolUse for the file-editing tools.
const REGISTRATIONS: [(&str, Option<&str>); 3] = [
    ("SessionStart", None),
    ("PreToolUse", Some("Edit|Write|MultiEdit")),
    ("PostToolUse", Some("Edit|Write|MultiEdit")),
];

fn is_our_hook(command: &str) -> bool {
    command.contains("jj-collect") && command.contains("hook")
}

/// Idempotently add our hooks. Returns true if anything changed.
pub fn merge_settings(path: &Path, command: &str) -> std::io::Result<bool> {
    let mut data: Value = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({}));
    if !data.is_object() {
        data = json!({});
    }
    let hooks = data
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let mut changed = false;

    for (event, matcher) in REGISTRATIONS {
        let groups = hooks
            .as_object_mut()
            .unwrap()
            .entry(event)
            .or_insert_with(|| json!([]));
        let arr = match groups.as_array_mut() {
            Some(a) => a,
            None => continue,
        };
        // Find a group with the same matcher.
        let existing = arr.iter_mut().find(|grp| group_matches(grp, matcher));
        if let Some(grp) = existing {
            let entry = grp
                .as_object_mut()
                .unwrap()
                .entry("hooks")
                .or_insert_with(|| json!([]));
            let list = entry.as_array_mut().unwrap();
            let present = list.iter().any(|h| {
                h.get("command").and_then(Value::as_str).map(is_our_hook).unwrap_or(false)
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
            grp.insert("hooks".into(), json!([{"type": "command", "command": command}]));
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
    let mut data: Value = match std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
        Some(v) => v,
        None => return Ok(0),
    };
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
                    !h.get("command").and_then(Value::as_str).map(is_our_hook).unwrap_or(false)
                });
                removed += before - list.len();
            }
        }
        groups.retain(|grp| {
            grp.get("hooks").and_then(Value::as_array).map(|l| !l.is_empty()).unwrap_or(false)
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

fn write_with_backup(path: &Path, data: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        if let Ok(orig) = std::fs::read(path) {
            let mut bak = path.as_os_str().to_owned();
            bak.push(".jj-collect-bak");
            let _ = std::fs::write(bak, orig);
        }
    }
    let mut text = serde_json::to_string_pretty(data)?;
    text.push('\n');
    std::fs::write(path, text)
}

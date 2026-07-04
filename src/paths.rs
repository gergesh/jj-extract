//! Repo discovery and the central data directory.
//!
//! jj-extract keeps its lock centrally under `~/.claude/jj-extract/<repo>-<hash>/`
//! rather than inside the repo, so nothing is littered into working trees.
//! `JJ_EXTRACT_HOME` relocates the root.

use std::path::{Path, PathBuf};

/// Nearest ancestor (inclusive) that is a jj working copy. jj-collect is
/// jj-native, so a plain-git repo without `.jj` is intentionally not a target.
pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let start = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    for dir in std::iter::once(start.as_path()).chain(start.ancestors().skip(1)) {
        if dir.join(".jj").exists() {
            return Some(dir.to_path_buf());
        }
    }
    None
}

/// Root of central storage: `$JJ_EXTRACT_HOME` or `~/.claude/jj-extract`.
pub fn central_root() -> PathBuf {
    if let Some(over) = std::env::var_os("JJ_EXTRACT_HOME") {
        return PathBuf::from(over);
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".claude").join("jj-extract")
}

/// Deterministic per-repo data dir: same repo root → same directory.
pub fn data_dir_for_root(root: &Path) -> PathBuf {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let slug = slug(root.file_name().and_then(|s| s.to_str()).unwrap_or("repo"));
    let digest = short_hash(&root.to_string_lossy());
    central_root().join(format!("{slug}-{digest}"))
}

/// Create the repo's data dir, dropping a `repo` back-pointer for `list --repos`.
pub fn ensure_data_dir(root: &Path) -> std::io::Result<PathBuf> {
    let base = data_dir_for_root(root);
    std::fs::create_dir_all(&base)?;
    let marker = base.join("repo");
    if !marker.exists() {
        let _ = std::fs::write(&marker, format!("{}\n", root.display()));
    }
    Ok(base)
}

/// Return `p` as a POSIX-style path relative to `root`, or None if outside it.
pub fn relpath_within(p: &str, root: &Path) -> Option<String> {
    let pp = Path::new(p);
    let abs = if pp.is_absolute() { pp.to_path_buf() } else { root.join(pp) };
    // Canonicalize the existing prefix so symlinks in the repo path don't defeat
    // the strip; fall back to lexical if the file was since deleted.
    let abs = abs.canonicalize().unwrap_or(abs);
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    abs.strip_prefix(&root).ok().map(|r| r.to_string_lossy().replace('\\', "/"))
}

fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "repo".into() } else { s }
}

/// Short, stable, dependency-free hash (FNV-1a, 64-bit) rendered as 10 hex chars.
fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")[..10].to_string()
}

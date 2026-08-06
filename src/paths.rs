//! Repo discovery. The edit lock lives in the repo's own `.jj/`, so there's no
//! per-repo data directory; `central_root` is only used for the best-effort hook
//! error log. `JJ_EXTRACT_HOME` relocates that root.

use std::path::{Path, PathBuf};

/// Nearest ancestor (inclusive) that is a jj working copy. jj-extract is
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

/// Root for the best-effort hook error log: `$JJ_EXTRACT_HOME` or `~/.jj-extract`.
pub fn central_root() -> PathBuf {
    if let Some(over) = std::env::var_os("JJ_EXTRACT_HOME") {
        return PathBuf::from(over);
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".jj-extract")
}

/// Return `p` as a POSIX-style path relative to `root`, or None if outside it.
pub fn relpath_within(p: &str, root: &Path) -> Option<String> {
    let pp = Path::new(p);
    let abs = if pp.is_absolute() {
        pp.to_path_buf()
    } else {
        root.join(pp)
    };
    // Canonicalize the existing prefix so symlinks in the repo path don't defeat
    // the strip; fall back to lexical if the file was since deleted.
    let abs = abs.canonicalize().unwrap_or(abs);
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let relative = abs.strip_prefix(&root).ok()?;
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(relative.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::relpath_within;

    #[test]
    fn rejects_nonexistent_paths_that_lexically_escape_the_repo() {
        let root = std::env::temp_dir().join("jj-extract-path-test-root");
        let escaped = root.join("..").join("outside").join("missing.txt");
        assert_eq!(relpath_within(&escaped.to_string_lossy(), &root), None);
    }

    #[test]
    fn accepts_repo_relative_paths() {
        let root = std::env::temp_dir().join("jj-extract-path-test-root");
        assert_eq!(
            relpath_within("src/main.rs", &root).as_deref(),
            Some("src/main.rs")
        );
    }
}

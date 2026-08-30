//! Repo discovery. The edit lock lives in the repo's own `.jj/`, so there's no
//! per-repo data directory; `central_root` is only used for the best-effort hook
//! error log. `JJ_EXTRACT_HOME` relocates that root.

use std::ffi::OsString;
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
    // Canonicalize both through their deepest existing ancestor, so symlinks in
    // the repo path can't defeat the strip for a path that isn't on disk — one a
    // tool is about to create, or has just deleted.
    let abs = canonical_prefix(&abs);
    let root = canonical_prefix(root);
    let relative = abs.strip_prefix(&root).ok()?;
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(relative.to_string_lossy().replace('\\', "/"))
}

/// `path` with its deepest existing ancestor canonicalized and the missing tail
/// re-attached. Plain `canonicalize` resolves nothing at all for a path that
/// doesn't exist, which would leave a symlinked ancestor unresolved.
fn canonical_prefix(path: &Path) -> PathBuf {
    let mut missing: Vec<OsString> = vec![];
    let mut existing = path.to_path_buf();
    loop {
        if let Ok(resolved) = existing.canonicalize() {
            return resolved.join(missing.iter().rev().collect::<PathBuf>());
        }
        match (existing.file_name().map(OsString::from), existing.parent()) {
            (Some(name), Some(parent)) => {
                missing.push(name);
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
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

    #[cfg(unix)]
    #[test]
    fn resolves_a_path_a_tool_is_about_to_create_under_a_symlinked_root() {
        let base = std::env::temp_dir().join("jj-extract-path-test-symlink");
        let real = base.join("real");
        let link = base.join("link");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&real).expect("create the real root");
        std::os::unix::fs::symlink(&real, &link).expect("link to the real root");

        let created = link.join("new.txt");
        assert_eq!(
            relpath_within(&created.to_string_lossy(), &real).as_deref(),
            Some("new.txt")
        );
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

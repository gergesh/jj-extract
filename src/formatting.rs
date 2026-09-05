//! Recognize Rust formatter-only edits without treating arbitrary whitespace
//! as disposable. Both versions must produce identical output from rustfmt.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::io::Write as _;
use std::process::{Command, Stdio};

use futures::AsyncReadExt as _;
use jj_lib::backend::{FileId, TreeValue};
use jj_lib::merged_tree::MergedTree;
use jj_lib::repo_path::RepoPathBuf;

const MAX_BYTES: u64 = 1024 * 1024;

thread_local! {
    // The cache belongs to one extraction process, not to the repository.
    static CACHE: RefCell<HashMap<FileId, Option<Vec<u8>>>> = RefCell::default();
}

pub async fn equivalent_paths(
    before: &MergedTree,
    after: &MergedTree,
    paths: &BTreeSet<RepoPathBuf>,
) -> Result<BTreeSet<RepoPathBuf>, String> {
    let mut equivalent = BTreeSet::new();
    for path in paths {
        if !path.as_internal_file_string().ends_with(".rs") {
            continue;
        }
        let old = before.path_value(path).await.map_err(|e| e.to_string())?;
        let new = after.path_value(path).await.map_err(|e| e.to_string())?;
        if let (
            Some(Some(TreeValue::File {
                id: old_id,
                executable: old_mode,
                copy_id: old_copy,
            })),
            Some(Some(TreeValue::File {
                id: new_id,
                executable: new_mode,
                copy_id: new_copy,
            })),
        ) = (old.as_resolved(), new.as_resolved())
        {
            if old_mode != new_mode || old_copy != new_copy {
                continue;
            }
            if let Some(old) = canonical(before, path, old_id).await? {
                if canonical(after, path, new_id).await?.as_ref() == Some(&old) {
                    equivalent.insert(path.clone());
                }
            }
        }
    }
    Ok(equivalent)
}

async fn canonical(
    tree: &MergedTree,
    path: &RepoPathBuf,
    id: &FileId,
) -> Result<Option<Vec<u8>>, String> {
    if let Some(cached) = CACHE.with(|cache| cache.borrow().get(id).cloned()) {
        return Ok(cached);
    }
    let mut bytes = Vec::new();
    tree.store()
        .read_file(path, id)
        .await
        .map_err(|e| e.to_string())?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| e.to_string())?;
    let result = if bytes.len() as u64 <= MAX_BYTES && !bytes.contains(&0) {
        format(&bytes)
    } else {
        None
    };
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= 256 {
            cache.clear();
        }
        cache.insert(id.clone(), result.clone());
    });
    Ok(result)
}

fn format(bytes: &[u8]) -> Option<Vec<u8>> {
    // Stdin/stdout only: no working files, project configuration, or child
    // modules are formatted. Missing rustfmt or invalid Rust disables this
    // optional optimization, leaving the ordinary conflict checks in charge.
    let mut child = Command::new("rustfmt")
        .args([
            "--edition",
            "2024",
            "--emit",
            "stdout",
            "--config-path",
            "/dev/null",
            "--config",
            "skip_children=true",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let written = child.stdin.take()?.write_all(bytes).is_ok();
    let output = child.wait_with_output().ok()?;
    (written && output.status.success()).then_some(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::format;

    #[test]
    fn formatting_preserves_literals_comments_and_code() {
        let compact =
            b"// keep this comment\nfn task()->usize{42}\nconst S:&str=\"two  spaces\";\n";
        let formatted = format(compact).expect("rustfmt is required by the test suite");
        assert_eq!(format(&formatted), Some(formatted.clone()));
        for changed in [
            String::from_utf8_lossy(compact).replace("42", "43"),
            String::from_utf8_lossy(compact).replace("two  spaces", "two spaces"),
            String::from_utf8_lossy(compact).replace("keep this comment", "a different comment"),
        ] {
            assert_ne!(format(changed.as_bytes()), Some(formatted.clone()));
        }
    }

    #[test]
    fn invalid_rust_cannot_be_classified_as_formatting() {
        assert!(format(b"fn broken(").is_none());
    }
}

//! Best-effort replay of optional context, including recognized formatter-only
//! edits. Conflicting context can be omitted; substantive edits use a full merge.

use futures::AsyncReadExt as _;
use jj_lib::backend::TreeValue;
use jj_lib::diff::ContentDiff;
use jj_lib::files::{merge_hunks, MergeResult};
use jj_lib::merge::{trivial_merge, Merge, SameChange};
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;

/// Keep the clean portions of a neutral rewrite, using the destination's bytes
/// for its conflicting hunks. In particular a repository-wide formatter may
/// depend on another agent's code in one part of a file, while formatting in a
/// different part supplies necessary context for this session's next edit.
///
/// Only resolved regular text files with unchanged modes qualify. Existing
/// conflicts, binary files, additions/deletions, symlinks and mode changes go
/// through the ordinary merge; this fallback never resolves their conflicts.
pub async fn keep_clean_hunks(
    destination: &MergedTree,
    before: &MergedTree,
    after: &MergedTree,
    merged: &MergedTree,
) -> Result<MergedTree, String> {
    const MAX_BYTES: u64 = 1024 * 1024;
    let store = destination.store();
    let mut builder = MergedTreeBuilder::new(merged.clone());
    for (path, value) in merged.conflicts() {
        value.map_err(|e| format!("could not inspect neutral conflict: {e}"))?;
        let mut contents = Vec::new();
        let mut metadata = None;
        for tree in [destination, before, after] {
            let value = tree
                .path_value(&path)
                .await
                .map_err(|e| format!("could not read neutral path: {e}"))?;
            let Some(Some(TreeValue::File {
                id,
                executable,
                copy_id,
            })) = value.as_resolved()
            else {
                break;
            };
            let file_metadata = (*executable, copy_id.clone());
            if metadata
                .as_ref()
                .is_some_and(|metadata| metadata != &file_metadata)
            {
                break;
            }
            metadata = Some(file_metadata);
            let mut bytes = Vec::new();
            store
                .read_file(&path, id)
                .await
                .map_err(|e| format!("could not read neutral file: {e}"))?
                .take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|e| format!("could not read neutral file content: {e}"))?;
            if bytes.len() as u64 > MAX_BYTES || bytes.contains(&0) {
                break;
            }
            contents.push(bytes);
        }
        if contents.len() != 3 {
            continue;
        }
        let bytes = match merge_hunks(&Merge::from_vec(contents), store.merge_options()) {
            MergeResult::Resolved(bytes) => bytes.to_vec(),
            MergeResult::Conflict(hunks) => {
                let mut bytes = Vec::new();
                for hunk in hunks {
                    if let Some(resolved) = hunk.as_resolved() {
                        bytes.extend_from_slice(resolved);
                    } else {
                        // jj deliberately leaves an entire line hunk unresolved
                        // if any word conflicts. Optional context can retain
                        // the independently applicable words in that hunk.
                        bytes.extend(partial_word_merge(&hunk));
                    }
                }
                bytes
            }
        };
        let id = store
            .write_file(&path, &mut bytes.as_slice())
            .await
            .map_err(|e| format!("could not write partial neutral context: {e}"))?;
        let (executable, copy_id) = metadata.unwrap();
        builder.set_or_remove(
            path,
            Merge::resolved(Some(TreeValue::File {
                id,
                executable,
                copy_id,
            })),
        );
    }
    builder
        .write_tree()
        .await
        .map_err(|e| format!("could not write partial neutral tree: {e}"))
}

fn partial_word_merge<T: AsRef<[u8]>>(inputs: &Merge<T>) -> Vec<u8> {
    // Diff against the recorded base, matching jj's file merge. Reorder each
    // hunk back to destination - before + after for the three-way decision.
    let diff = ContentDiff::by_word(inputs.removes().chain(inputs.adds()));
    let mut bytes = Vec::new();
    for hunk in diff.hunks() {
        let terms = [hunk.contents[1], hunk.contents[0], hunk.contents[2]];
        bytes.extend_from_slice(trivial_merge(&terms, SameChange::Accept).unwrap_or(&terms[0]));
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_formatting_can_commute_in_part_of_a_conflicting_line_hunk() {
        let inputs = Merge::from_vec(vec![
            "const x = 3;\nconst y = 2;\n",
            "const x = 3;\nconst other = 9;\n",
            "const x=3;\nconst other=9;\n",
        ]);
        let merged = String::from_utf8(partial_word_merge(&inputs)).unwrap();
        assert!(merged.contains("const x=3;"));
        assert!(merged.contains('y') && merged.contains('2'));
        assert!(!merged.contains("other") && !merged.contains('9'));
    }

    #[test]
    fn conflicting_neutral_content_keeps_the_destination() {
        let inputs = Merge::from_vec(vec![
            "value = agent;\n",
            "value = old;\n",
            "value = human;\n",
        ]);
        assert_eq!(partial_word_merge(&inputs), b"value = agent;\n");
    }
}

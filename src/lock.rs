//! A cross-process exclusive lock, held for the duration of every mutation of a
//! repo's collect stack. This is the backbone of race-safety: concurrent agent
//! hooks serialize here, so no two of them manipulate the shared jj working copy
//! (`@`) at the same time. Combined with path-scoped squashes (each hook claims
//! only its own files, leaving a peer's edits in `@`), simultaneous edits to
//! different files never collide.

use fs2::FileExt;
use std::fs::File;
use std::path::Path;

/// RAII guard: holds an exclusive `flock` until dropped.
pub struct RepoLock {
    file: File,
}

impl RepoLock {
    /// Acquire the exclusive lock for a repo's data dir, blocking until free.
    pub fn acquire(base: &Path) -> std::io::Result<RepoLock> {
        std::fs::create_dir_all(base)?;
        let file = File::create(base.join("collect.lock"))?;
        file.lock_exclusive()?;
        Ok(RepoLock { file })
    }
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

//! The edit lock: a lockfile held across a whole edit — PreToolUse takes it,
//! PostToolUse releases it — so an agent's Pre → write → Post runs to completion
//! before any other agent's edit can start. A blocked PreToolUse blocks the tool
//! itself, so a peer literally cannot write while another edit is in flight; that
//! closes the same-file race a per-hook lock can't (both writes would otherwise
//! already be merged on disk before either hook runs).
//!
//! It must be a *file* lock, not an `flock`: PreToolUse and PostToolUse are
//! separate processes, and an `flock` releases the instant PreToolUse exits.
//!
//! A lockfile isn't freed when a process dies, so a tool that is denied or
//! crashes (no PostToolUse) would wedge all editing forever. So the lock is
//! time-bounded: a holder older than `STALE_SECS` is treated as dead and stolen.
//! Edit tool executions are milliseconds, so the window is safe.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A holder older than this many seconds is treated as dead and its lock stolen.
const STALE_SECS: u64 = 10;
/// Never block a tool longer than this, even while losing steal races.
const MAX_WAIT_MS: u64 = 12_000;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn lock_path(base: &Path) -> PathBuf {
    base.join("edit.lock")
}

/// Take the edit lock for `holder`, stealing a stale holder and — to never wedge
/// the tool — stealing outright after `MAX_WAIT_MS`. The lock persists (survives
/// this process) until [`release`].
pub fn acquire(base: &Path, holder: &str) {
    let _ = fs::create_dir_all(base);
    let path = lock_path(base);
    let mut waited = 0u64;
    loop {
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(mut f) => {
                let _ = write!(f, "{} {}", now(), holder);
                return;
            }
            Err(_) => {
                if lock_is_stale(&path) || waited >= MAX_WAIT_MS {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                sleep(Duration::from_millis(25));
                waited += 25;
            }
        }
    }
}

/// Release the edit lock iff we still hold it (a stolen lock is left alone).
pub fn release(base: &Path, holder: &str) {
    let path = lock_path(base);
    if read_lock(&path).map(|(_, who)| who).as_deref() == Some(holder) {
        let _ = fs::remove_file(&path);
    }
}

/// RAII holder for single-process use (`jj collect`): acquires on construction,
/// releases on drop. The hooks instead acquire (Pre) and release (Post) across
/// two processes, so they can't use this.
pub struct Guard {
    base: PathBuf,
    holder: String,
}

impl Guard {
    pub fn new(base: &Path, holder: &str) -> Guard {
        acquire(base, holder);
        Guard { base: base.to_path_buf(), holder: holder.to_string() }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        release(&self.base, &self.holder);
    }
}

fn read_lock(path: &Path) -> Option<(u64, String)> {
    let mut s = String::new();
    OpenOptions::new().read(true).open(path).ok()?.read_to_string(&mut s).ok()?;
    let mut it = s.split_whitespace();
    let ts = it.next()?.parse::<u64>().ok()?;
    let who = it.next().unwrap_or("").to_string();
    Some((ts, who))
}

fn lock_is_stale(path: &Path) -> bool {
    match read_lock(path) {
        Some((ts, _)) => now().saturating_sub(ts) > STALE_SECS,
        None => true, // unreadable/garbage → steal
    }
}

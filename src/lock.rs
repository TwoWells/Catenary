// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Filesystem-based advisory file locks for concurrent agent coordination.
//!
//! When multiple AI agents (separate Claude Code sessions, Gemini CLI instances,
//! or background subagents) edit files in the same workspace, their edits can
//! interleave destructively. This module provides file-level advisory locks that
//! serialize access across all agents on the machine.
//!
//! Lock state lives on the filesystem so it is visible across multiple Catenary
//! instances. The lock primitive is atomic rename (`temp file` → `lock file`),
//! which is atomic on POSIX and Windows (Rust 1.78+ uses
//! `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`).
//!
//! ## Lock lifecycle
//!
//! 1. **Acquire** (PreToolUse hook): agent claims the lock before editing.
//! 2. **Release with grace** (PostToolUse hook): lock enters a grace period
//!    (default 30s) allowing the same agent to re-acquire without contention.
//! 3. **Grace expiry**: after the grace period, any agent can reclaim the lock.
//! 4. **Stale recovery**: if `last_activity` exceeds the staleness threshold,
//!    the lock is considered abandoned.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Default timeout for lock acquisition (seconds).
pub const DEFAULT_TIMEOUT_SECS: u64 = 180;

/// Default grace period after release (seconds).
pub const DEFAULT_GRACE_SECS: u64 = 30;

/// Default poll interval (milliseconds).
const POLL_INTERVAL_MS: u64 = 500;

/// Extra time beyond timeout + grace before a lock is considered stale (seconds).
const STALENESS_MARGIN_SECS: u64 = 60;

/// Manages file-level advisory locks on the filesystem.
///
/// Each managed file gets a lock file in the locks directory, named by a
/// deterministic hash of the absolute file path.
pub struct FileLockManager {
    locks_dir: PathBuf,
}

/// Persistent lock state stored as JSON on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockState {
    /// Owner identity (e.g. `"session_id"` or `"session_id:agent_id"`).
    pub owner: String,
    /// Absolute path of the locked file (for human readability).
    pub file_path: String,
    /// Unix timestamp when the lock was first acquired.
    pub acquired_at: u64,
    /// Unix timestamp after which the lock can be reclaimed by another owner.
    /// `None` means the lock is actively held (not in grace period).
    pub grace_until: Option<u64>,
    /// Unix timestamp of the most recent acquire/refresh by the owner.
    pub last_activity: u64,
}

/// Result of an acquire attempt.
#[derive(Debug)]
pub enum AcquireResult {
    /// Lock acquired (or already held by this owner).
    Acquired,
    /// Lock acquired, but the file was modified since the owner last read it.
    AcquiredStaleRead {
        /// Human-readable warning message.
        context: String,
    },
    /// Timed out waiting for a lock held by another owner.
    Denied {
        /// Human-readable reason for denial.
        reason: String,
    },
}

/// Read-tracking entry for change detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReadTrack {
    /// File modification time as milliseconds since UNIX epoch.
    mtime_ms: u64,
}

impl FileLockManager {
    /// Creates a new `FileLockManager` using the default locks directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the locks directory cannot be created.
    pub fn new() -> Result<Self> {
        let locks_dir = locks_dir();
        std::fs::create_dir_all(&locks_dir).map_err(|e| {
            anyhow!(
                "Failed to create locks directory {}: {e}",
                locks_dir.display()
            )
        })?;
        Ok(Self { locks_dir })
    }

    /// Creates a `FileLockManager` with a custom locks directory (for testing).
    ///
    /// # Errors
    ///
    /// Returns an error if the locks directory cannot be created.
    #[cfg(test)]
    pub fn with_dir(locks_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&locks_dir)?;
        Ok(Self { locks_dir })
    }

    /// Attempts to acquire a lock on the given file.
    ///
    /// Blocks (polls) until the lock is available or the timeout expires.
    ///
    /// # Arguments
    ///
    /// * `file_path` — Absolute path to the file being locked.
    /// * `owner` — Identity of the agent acquiring the lock.
    /// * `timeout_secs` — Maximum time to wait for the lock.
    #[must_use]
    pub fn acquire(&self, file_path: &str, owner: &str, timeout_secs: u64) -> AcquireResult {
        let lock_path = self.lock_path(file_path);
        let now = unix_now();
        let deadline = now + timeout_secs;
        let staleness_threshold =
            now.saturating_sub(timeout_secs + DEFAULT_GRACE_SECS + STALENESS_MARGIN_SECS);

        loop {
            let now = unix_now();
            if now >= deadline {
                // Read the lock one more time for the denial message
                if let Some(state) = self.read_lock(&lock_path) {
                    let held_secs = now.saturating_sub(state.acquired_at);
                    return AcquireResult::Denied {
                        reason: format!(
                            "File {file_path} is locked by another agent (held for {held_secs}s). \
                             Work on a different file and retry later."
                        ),
                    };
                }
                // Lock disappeared between check and timeout — try once more
                if self.try_claim(&lock_path, file_path, owner, now) {
                    return self.check_stale_read(file_path, owner);
                }
                return AcquireResult::Denied {
                    reason: format!("File {file_path} lock acquisition timed out. Retry later."),
                };
            }

            match self.read_lock(&lock_path) {
                None => {
                    // No lock — claim it
                    if self.try_claim(&lock_path, file_path, owner, now) {
                        return self.check_stale_read(file_path, owner);
                    }
                    // Someone else claimed between read and write — retry
                }
                Some(state) if state.owner == owner => {
                    // Same owner — refresh
                    if self.try_claim(&lock_path, file_path, owner, now) {
                        return self.check_stale_read(file_path, owner);
                    }
                }
                Some(state) => {
                    // Different owner — check if reclaimable
                    let reclaimable = state.grace_until.is_some_and(|g| now >= g)
                        || state.last_activity < staleness_threshold;

                    if reclaimable && self.try_claim(&lock_path, file_path, owner, now) {
                        return self.check_stale_read(file_path, owner);
                    }
                    // Still locked — wait and retry
                }
            }

            std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        }
    }

    /// Releases a lock with an optional grace period.
    ///
    /// If `grace_secs` is 0, the lock file is removed immediately.
    /// Otherwise, `grace_until` is set to `now + grace_secs`.
    ///
    /// # Errors
    ///
    /// Returns an error if the lock file cannot be updated.
    pub fn release(&self, file_path: &str, owner: &str, grace_secs: u64) -> Result<()> {
        let lock_path = self.lock_path(file_path);

        let Some(state) = self.read_lock(&lock_path) else {
            // No lock file — nothing to release
            return Ok(());
        };

        if state.owner != owner {
            // Not our lock — don't touch it
            return Ok(());
        }

        if grace_secs == 0 {
            // Immediate release
            let _ = std::fs::remove_file(&lock_path);
            return Ok(());
        }

        let now = unix_now();
        let updated = LockState {
            grace_until: Some(now + grace_secs),
            last_activity: now,
            ..state
        };

        self.atomic_write(&lock_path, &updated)
    }

    /// Records the current modification time of a file for change detection.
    ///
    /// # Errors
    ///
    /// Returns an error if the tracking file cannot be written.
    pub fn track_read(&self, file_path: &str, owner: &str) -> Result<()> {
        let mtime = file_mtime_ms(file_path).unwrap_or(0);
        let track = ReadTrack { mtime_ms: mtime };

        let reads_dir = self.reads_dir(file_path);
        std::fs::create_dir_all(&reads_dir)?;

        let track_path = reads_dir.join(format!("{}.json", fnv1a_hash(owner)));
        let bytes = serde_json::to_vec(&track)
            .map_err(|e| anyhow!("Failed to serialize read track: {e}"))?;
        self.atomic_write_bytes(&track_path, &bytes)
    }

    /// Returns the lock file path for a given file.
    fn lock_path(&self, file_path: &str) -> PathBuf {
        self.locks_dir
            .join(format!("{}.json", fnv1a_hash(file_path)))
    }

    /// Returns the reads tracking directory for a given file.
    fn reads_dir(&self, file_path: &str) -> PathBuf {
        self.locks_dir
            .join(format!("{}.reads", fnv1a_hash(file_path)))
    }

    /// Reads and deserializes a lock file. Returns `None` on any error.
    #[allow(clippy::unused_self, reason = "Method on manager for API consistency")]
    fn read_lock(&self, lock_path: &Path) -> Option<LockState> {
        let data = std::fs::read_to_string(lock_path).ok()?;
        serde_json::from_str(&data).ok()
    }

    /// Attempts to claim a lock via atomic write. Returns `true` on success.
    fn try_claim(&self, lock_path: &Path, file_path: &str, owner: &str, now: u64) -> bool {
        let state = LockState {
            owner: owner.to_string(),
            file_path: file_path.to_string(),
            acquired_at: now,
            grace_until: None,
            last_activity: now,
        };

        self.atomic_write(lock_path, &state).is_ok()
    }

    /// Checks if the file was modified since the owner's last tracked read.
    fn check_stale_read(&self, file_path: &str, owner: &str) -> AcquireResult {
        let track_path = self
            .reads_dir(file_path)
            .join(format!("{}.json", fnv1a_hash(owner)));

        let Some(data) = std::fs::read_to_string(&track_path).ok() else {
            // No read tracking — can't detect staleness
            return AcquireResult::Acquired;
        };

        let Some(track) = serde_json::from_str::<ReadTrack>(&data).ok() else {
            return AcquireResult::Acquired;
        };

        let current_mtime = file_mtime_ms(file_path).unwrap_or(0);

        if track.mtime_ms != 0 && current_mtime != track.mtime_ms {
            AcquireResult::AcquiredStaleRead {
                context: format!(
                    "Warning: {file_path} was modified by another agent since you last \
                     read it. Re-read the file before editing to avoid overwriting changes."
                ),
            }
        } else {
            AcquireResult::Acquired
        }
    }

    /// Atomically writes a `LockState` to a file via temp + rename.
    fn atomic_write(&self, path: &Path, state: &LockState) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(state).map_err(|e| anyhow!("JSON serialize: {e}"))?;
        self.atomic_write_bytes(path, &bytes)
    }

    /// Atomically writes bytes to a file via temp + rename.
    #[allow(clippy::unused_self, reason = "Method on manager for API consistency")]
    fn atomic_write_bytes(&self, path: &Path, data: &[u8]) -> Result<()> {
        let pid = std::process::id();
        let temp_path = path.with_extension(format!("tmp.{pid}"));

        std::fs::write(&temp_path, data).map_err(|e| {
            anyhow!(
                "Failed to write temp lock file {}: {e}",
                temp_path.display()
            )
        })?;

        std::fs::rename(&temp_path, path).map_err(|e| {
            // Clean up temp file on rename failure
            let _ = std::fs::remove_file(&temp_path);
            anyhow!(
                "Failed to rename {} -> {}: {e}",
                temp_path.display(),
                path.display()
            )
        })
    }
}

/// Returns the base directory for lock files.
///
/// Uses the same directory resolution as `session::sessions_dir()`:
/// `$XDG_STATE_HOME/catenary/locks/` with fallback to `$XDG_DATA_HOME` or `/tmp`.
pub fn locks_dir() -> PathBuf {
    let state_dir = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    state_dir.join("catenary").join("locks")
}

/// Computes a deterministic FNV-1a 64-bit hash of a string, returned as 16 hex
/// characters.
///
/// This is used for mapping file paths and owner identities to lock file names.
/// Cryptographic strength is not needed — we only need determinism and low
/// collision probability for paths on a single machine.
fn fnv1a_hash(input: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;

    let mut hash: u64 = FNV_OFFSET;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

/// Returns the current time as seconds since UNIX epoch.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Returns the modification time of a file as milliseconds since UNIX epoch.
fn file_mtime_ms(path: &str) -> Option<u64> {
    let duration = std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?;
    // Saturate at u64::MAX (year ~584 million) — safe for any real file mtime.
    Some(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests use unwrap for brevity")]
mod tests {
    use super::*;

    fn setup() -> (FileLockManager, tempfile::TempDir) {
        let dir = tempfile::tempdir().ok().unwrap();
        let mgr = FileLockManager::with_dir(dir.path().join("locks"))
            .ok()
            .unwrap();
        (mgr, dir)
    }

    #[test]
    fn acquire_uncontested() {
        let (mgr, _dir) = setup();
        let result = mgr.acquire("/tmp/test.rs", "agent-a", 5);
        assert!(matches!(result, AcquireResult::Acquired));
    }

    #[test]
    fn reacquire_same_owner() {
        let (mgr, _dir) = setup();
        let result = mgr.acquire("/tmp/test.rs", "agent-a", 5);
        assert!(matches!(result, AcquireResult::Acquired));

        // Release with grace
        mgr.release("/tmp/test.rs", "agent-a", 30).ok().unwrap();

        // Re-acquire by same owner should succeed immediately
        let result = mgr.acquire("/tmp/test.rs", "agent-a", 5);
        assert!(matches!(result, AcquireResult::Acquired));
    }

    #[test]
    fn blocked_by_different_owner() {
        let (mgr, _dir) = setup();

        // Agent A acquires
        let result = mgr.acquire("/tmp/test.rs", "agent-a", 5);
        assert!(matches!(result, AcquireResult::Acquired));

        // Agent B should be denied (lock is active, no grace, 1s timeout)
        let result = mgr.acquire("/tmp/test.rs", "agent-b", 1);
        assert!(matches!(result, AcquireResult::Denied { .. }));
    }

    #[test]
    fn grace_period_allows_reclaim() {
        let (mgr, _dir) = setup();

        // Agent A acquires
        let result = mgr.acquire("/tmp/test.rs", "agent-a", 5);
        assert!(matches!(result, AcquireResult::Acquired));

        // Agent A releases with 0 grace (immediate)
        mgr.release("/tmp/test.rs", "agent-a", 0).ok().unwrap();

        // Agent B should acquire immediately
        let result = mgr.acquire("/tmp/test.rs", "agent-b", 1);
        assert!(matches!(result, AcquireResult::Acquired));
    }

    #[test]
    fn grace_period_blocks_until_expired() {
        let (mgr, _dir) = setup();

        // Agent A acquires and releases with 1s grace
        let result = mgr.acquire("/tmp/test.rs", "agent-a", 5);
        assert!(matches!(result, AcquireResult::Acquired));
        mgr.release("/tmp/test.rs", "agent-a", 1).ok().unwrap();

        // Agent B tries immediately — lock is in grace, should wait then succeed
        // With 1s grace and 5s timeout, B should get it after ~1s
        let start = std::time::Instant::now();
        let result = mgr.acquire("/tmp/test.rs", "agent-b", 5);
        let elapsed = start.elapsed();

        assert!(matches!(result, AcquireResult::Acquired));
        // Should have waited at least ~500ms (one poll interval)
        assert!(
            elapsed >= Duration::from_millis(400),
            "Expected wait, got {elapsed:?}"
        );
    }

    #[test]
    fn release_not_owner_is_noop() {
        let (mgr, _dir) = setup();

        // Agent A acquires
        let result = mgr.acquire("/tmp/test.rs", "agent-a", 5);
        assert!(matches!(result, AcquireResult::Acquired));

        // Agent B tries to release — should be a no-op
        mgr.release("/tmp/test.rs", "agent-b", 0).ok().unwrap();

        // Agent A's lock should still be active
        let result = mgr.acquire("/tmp/test.rs", "agent-b", 1);
        assert!(matches!(result, AcquireResult::Denied { .. }));
    }

    #[test]
    fn release_nonexistent_is_ok() {
        let (mgr, _dir) = setup();
        // Releasing a file that was never locked should succeed silently
        mgr.release("/tmp/nonexistent.rs", "agent-a", 0)
            .ok()
            .unwrap();
    }

    #[test]
    fn read_tracking_detects_change() {
        let (mgr, dir) = setup();

        // Create a real file to track
        let test_file = dir.path().join("tracked.rs");
        std::fs::write(&test_file, "original content").ok().unwrap();
        let file_str = test_file.to_string_lossy().to_string();

        // Track the read
        mgr.track_read(&file_str, "agent-a").ok().unwrap();

        // Modify the file (change mtime)
        std::thread::sleep(Duration::from_millis(50));
        std::fs::write(&test_file, "modified content").ok().unwrap();

        // Acquire should detect the stale read
        let result = mgr.acquire(&file_str, "agent-a", 5);
        assert!(
            matches!(result, AcquireResult::AcquiredStaleRead { .. }),
            "Expected AcquiredStaleRead, got {result:?}"
        );
    }

    #[test]
    fn read_tracking_no_change() {
        let (mgr, dir) = setup();

        // Create a real file to track
        let test_file = dir.path().join("stable.rs");
        std::fs::write(&test_file, "content").ok().unwrap();
        let file_str = test_file.to_string_lossy().to_string();

        // Track the read
        mgr.track_read(&file_str, "agent-a").ok().unwrap();

        // Acquire without modifying — should be clean
        let result = mgr.acquire(&file_str, "agent-a", 5);
        assert!(matches!(result, AcquireResult::Acquired));
    }

    #[test]
    fn fnv1a_hash_deterministic() {
        let h1 = fnv1a_hash("/home/user/project/src/main.rs");
        let h2 = fnv1a_hash("/home/user/project/src/main.rs");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);

        // Different paths produce different hashes
        let h3 = fnv1a_hash("/home/user/project/src/lib.rs");
        assert_ne!(h1, h3);
    }
}

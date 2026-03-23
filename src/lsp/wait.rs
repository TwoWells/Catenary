// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Unified wait infrastructure for load-aware failure detection.
//!
//! `load_aware_grace` is the single wait pattern used by all sites that
//! wait on an LSP server process — diagnostics preamble, readiness,
//! request timeouts. It replaces wall-clock timeouts with CPU-tick
//! failure detection.

use catenary_proc::ProcessDelta;
use std::future::Future;
use std::time::Duration;
use tokio::sync::Notify;

/// Poll interval for failure detection sampling.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Wall-clock safety cap for pathological cases (D-state, NFS hang, zombies).
///
/// This should never fire under normal operation. It exists because CPU
/// ticks cannot advance in these states, so the failure detector alone
/// would wait forever.
const SAFETY_CAP: Duration = Duration::from_secs(300);

/// Wait for a condition, using failure detection to catch stuck servers.
///
/// Wakes immediately on `notify` (event-driven), samples ticks on
/// `POLL_INTERVAL` (failure detection). Returns `true` if the condition
/// was met, `false` if the server is stuck or dead.
///
/// The failure threshold only counts **unexplained** CPU consumption:
/// Running + ticks advancing + no active progress. Explained work
/// (progress Active) and free waits (sleeping, blocked, starved) do
/// not count against the threshold.
///
/// # Parameters
///
/// - `sample_fn`: Samples the server process. Returns [`ProcessDelta`].
///   Returns `None` when the process is gone.
/// - `threshold`: CPU ticks of unexplained work before giving up.
/// - `max_wall`: Maximum wall-clock time to wait. Use `None` for the
///   default 5-minute safety cap.
/// - `notify`: Wakes the loop when the condition might have changed.
/// - `progress_active`: Returns `true` when the server has active
///   `$/progress` tokens — ticks during active progress are explained.
/// - `condition`: Checked on each wake. Returns `true` to exit successfully.
pub async fn load_aware_grace<S, F, Fut>(
    sample_fn: &mut S,
    threshold: u64,
    max_wall: Option<Duration>,
    notify: &Notify,
    progress_active: impl Fn() -> bool,
    condition: F,
) -> bool
where
    S: FnMut() -> Option<ProcessDelta>,
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    let wall_deadline = tokio::time::Instant::now() + max_wall.unwrap_or(SAFETY_CAP);
    let mut remaining_threshold = i64::try_from(threshold).unwrap_or(i64::MAX);

    loop {
        // Check condition first — may already be satisfied
        if condition().await {
            return true;
        }

        // Wait for either an event or the poll interval
        tokio::select! {
            () = notify.notified() => {
                // Event fired — check condition immediately
                if condition().await {
                    return true;
                }
                // Condition not met yet, continue to failure detection below
            }
            () = tokio::time::sleep(POLL_INTERVAL) => {
                // Poll interval elapsed — run failure detection
            }
        }

        // Sample the server process
        let Some(d) = sample_fn() else {
            // Can't sample — process is gone
            return false;
        };

        match d.state {
            catenary_proc::ProcessState::Dead => return false,
            catenary_proc::ProcessState::Blocked => {
                // Kernel I/O — free wait, don't drain threshold
            }
            catenary_proc::ProcessState::Running | catenary_proc::ProcessState::Sleeping => {
                // Only count unexplained work: Running + ticks advanced + no progress
                let delta = d.delta_utime + d.delta_stime;
                if d.state == catenary_proc::ProcessState::Running
                    && delta > 0
                    && !progress_active()
                {
                    remaining_threshold -= i64::try_from(delta).unwrap_or(remaining_threshold);
                }
                // Sleeping + flat ticks, Running + flat ticks, or progress Active:
                // all free waits — don't drain threshold.
            }
        }

        if remaining_threshold <= 0 {
            return false;
        }

        if tokio::time::Instant::now() >= wall_deadline {
            return false;
        }
    }
}

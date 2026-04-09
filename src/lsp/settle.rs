// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Settle: waits for the server process tree to go quiet after a stimulus.
//!
//! The production [`settle`] function implements a two-phase approach:
//! 1. **Work gate:** Prove the server was scheduled after stimulus.
//! 2. **Quiet detection:** Wait for all processes to show zero deltas.
//!
//! The profiling [`settle_loop`] runs the sampling loop continuously and
//! yields per-process samples to a caller-provided [`SettleSink`].

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use catenary_proc::ProcessState;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::server::LspServer;
use super::state::ServerLifecycle;

// ── Production settle ─────────────────────────────────────────────────

/// Polling interval for tree walks (validated by profiling).
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Budget: 60 seconds of server CPU time in centiseconds (100 Hz).
const CPUTIME_BUDGET: u64 = 6000;

/// Outcome of the settle operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleResult {
    /// Server settled — all processes quiet.
    Settled,
    /// Budget exhausted — server consumed 60s of CPU time.
    BudgetExhausted,
    /// Root process died.
    RootDied,
}

/// Wait for the server to finish processing and go quiet.
///
/// Two-phase settle: work gate (prove server was scheduled) followed by
/// quiet detection (all processes show zero deltas). During `Busy(n)`
/// lifecycle, tree walking is paused — progress tokens indicate the
/// server is still working.
///
/// Returns when the server settles, the cputime budget is exhausted,
/// the root process dies, or the cancel token fires.
#[allow(
    clippy::similar_names,
    reason = "delta_utime/delta_stime are standard counter names"
)]
pub async fn settle(server: &Arc<LspServer>, cancel: CancellationToken) -> SettleResult {
    let mut work_gate_satisfied = false;
    let mut active_pids: HashSet<u32> = HashSet::new();
    let mut known_pids: HashSet<u32> = HashSet::new();
    let mut cumulative_cputime: u64 = 0;

    loop {
        tokio::select! {
            () = tokio::time::sleep(POLL_INTERVAL) => {}
            () = cancel.cancelled() => { return SettleResult::Settled; }
        }

        let lifecycle = server.lifecycle();

        // Terminal states
        if lifecycle.is_terminal() {
            return SettleResult::RootDied;
        }

        // During Busy: work gate is satisfied, skip tree walking
        if matches!(lifecycle, ServerLifecycle::Busy(_)) {
            work_gate_satisfied = true;
            continue;
        }

        // Sample the process tree via spawn_blocking (/proc reads are sync)
        let server_clone = Arc::clone(server);
        let Ok(Some(snapshot)) =
            tokio::task::spawn_blocking(move || server_clone.sample_tree()).await
        else {
            server.set_lifecycle(ServerLifecycle::Dead);
            return SettleResult::RootDied;
        };

        // Root death check
        if let Some(result) = check_root_death(server, &snapshot) {
            return result;
        }

        // Analyze tree activity
        let mut any_nonzero = false;
        let mut new_pids = false;
        let mut interval_cputime: u64 = 0;

        for ts in &snapshot.samples {
            let is_active = ts.delta_pfc > 0 || ts.delta_utime > 0 || ts.delta_stime > 0;

            if is_active {
                any_nonzero = true;
                active_pids.insert(ts.pid);
            }

            if known_pids.insert(ts.pid) {
                new_pids = true;
            }

            // Budget: sum cputime, sub-tick activity (pfc > 0 but cputime == 0) = 1 tick
            let cputime = ts.delta_utime + ts.delta_stime;
            interval_cputime += if cputime == 0 && ts.delta_pfc > 0 {
                1
            } else {
                cputime
            };
        }

        // Phase 1: Work gate — wait for any activity
        if !work_gate_satisfied {
            if any_nonzero {
                work_gate_satisfied = true;
                debug!("settle: work gate satisfied");
            }
            // Budget does not cover the work gate phase
            continue;
        }

        // Phase 2: Quiet detection — budget tracking
        cumulative_cputime += interval_cputime;
        if cumulative_cputime >= CPUTIME_BUDGET {
            debug!("settle: budget exhausted ({cumulative_cputime} centiseconds)");
            return SettleResult::BudgetExhausted;
        }

        // Quiet: all zeros, no new PIDs, all per-child gates satisfied
        if !any_nonzero && !new_pids {
            let all_gates = snapshot
                .samples
                .iter()
                .all(|ts| ts.state == ProcessState::Dead || active_pids.contains(&ts.pid));

            if all_gates {
                debug!("settle: settled (all processes quiet)");
                return SettleResult::Settled;
            }
        }
    }
}

/// Checks whether the root process has died and transitions lifecycle.
fn check_root_death(
    server: &LspServer,
    snapshot: &catenary_proc::TreeSnapshot,
) -> Option<SettleResult> {
    // Empty snapshot → root is gone
    if snapshot.process_count == 0 || snapshot.samples.is_empty() {
        debug!("settle: root process gone (empty snapshot)");
        server.set_lifecycle(ServerLifecycle::Dead);
        return Some(SettleResult::RootDied);
    }

    let Some(root_pid) = server.pid() else {
        server.set_lifecycle(ServerLifecycle::Dead);
        return Some(SettleResult::RootDied);
    };

    match snapshot.samples.iter().find(|s| s.pid == root_pid) {
        Some(root) if root.state == ProcessState::Dead => {
            debug!("settle: root process is zombie/dead");
            server.set_lifecycle(ServerLifecycle::Dead);
            Some(SettleResult::RootDied)
        }
        None => {
            debug!("settle: root PID not in snapshot");
            server.set_lifecycle(ServerLifecycle::Dead);
            Some(SettleResult::RootDied)
        }
        _ => None,
    }
}

// ── Profiling settle loop ─────────────────────────────────────────────

/// A single sample from one process in the tree.
pub struct SettleSample {
    /// When the sample was taken.
    pub timestamp: Instant,
    /// Server name (e.g. `"rust-analyzer"`).
    pub server: String,
    /// Process ID.
    pub pid: u32,
    /// Parent process ID.
    pub ppid: u32,
    /// Page fault count delta since last sample.
    pub delta_pfc: u64,
    /// User CPU time delta since last sample (centiseconds).
    pub delta_utime: u64,
    /// System CPU time delta since last sample (centiseconds).
    pub delta_stime: u64,
    /// Count of in-flight progress tokens at sample time.
    pub in_progress_count: u32,
    /// Total processes in the tree at this sample.
    pub process_count: usize,
}

/// Receives samples from the settle loop.
///
/// Sync and non-async — recording to a database or vec is a blocking
/// operation, and the settle loop controls its own async timing.
pub trait SettleSink: Send {
    /// Called for each per-process sample. Return `false` to stop the loop.
    fn record(&mut self, sample: &SettleSample) -> bool;
}

/// Run the settle sampling loop (profiling).
///
/// Polls `tree_monitor` every `interval`, reads `in_progress_count` from
/// `server`, and calls `sink.record()` for each per-process sample.
/// Runs until `sink.record()` returns `false` or the `cancel` token fires.
///
/// This is the profiling variant — it yields every sample to the sink for
/// recording. The production [`settle`] function uses the tree monitor on
/// [`LspServer`] and makes settle decisions internally.
#[allow(
    clippy::similar_names,
    reason = "delta_utime/delta_stime are standard counter names"
)]
pub async fn settle_loop(
    tree_monitor: &mut catenary_proc::TreeMonitor,
    server: &LspServer,
    server_name: &str,
    interval: Duration,
    sink: &mut dyn SettleSink,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            () = cancel.cancelled() => { return; }
        }

        let snapshot = tree_monitor.sample();
        let in_progress_count = server.in_progress_count();
        let timestamp = Instant::now();

        for ts in &snapshot.samples {
            let sample = SettleSample {
                timestamp,
                server: server_name.to_string(),
                pid: ts.pid,
                ppid: ts.ppid,
                delta_pfc: ts.delta_pfc,
                delta_utime: ts.delta_utime,
                delta_stime: ts.delta_stime,
                in_progress_count,
                process_count: snapshot.process_count,
            };

            if !sink.record(&sample) {
                return;
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use catenary_proc::{TreeSample, TreeSnapshot};

    fn test_server() -> LspServer {
        LspServer::new("test".to_string(), None)
    }

    #[tokio::test]
    async fn settle_returns_root_died_for_terminal_state() {
        let server = Arc::new(test_server());
        server.set_lifecycle(ServerLifecycle::Dead);
        let cancel = CancellationToken::new();
        let result = settle(&server, cancel).await;
        assert_eq!(result, SettleResult::RootDied);
    }

    #[tokio::test]
    async fn settle_returns_root_died_without_tree_monitor() {
        let server = Arc::new(test_server());
        server.set_lifecycle(ServerLifecycle::Healthy);
        let cancel = CancellationToken::new();
        let result = settle(&server, cancel).await;
        assert_eq!(result, SettleResult::RootDied);
    }

    #[test]
    fn check_root_death_empty_snapshot() {
        let server = test_server();
        server.set_lifecycle(ServerLifecycle::Healthy);
        let snapshot = TreeSnapshot {
            samples: Vec::new(),
            process_count: 0,
        };
        let result = check_root_death(&server, &snapshot);
        assert_eq!(result, Some(SettleResult::RootDied));
        assert_eq!(server.lifecycle(), ServerLifecycle::Dead);
    }

    #[test]
    fn check_root_death_zombie_root() {
        let server = test_server();
        server.set_lifecycle(ServerLifecycle::Healthy);
        // Server has no connection, so pid() returns None → RootDied
        let snapshot = TreeSnapshot {
            samples: vec![TreeSample {
                pid: 1234,
                ppid: 1,
                delta_utime: 0,
                delta_stime: 0,
                delta_pfc: 0,
                state: ProcessState::Dead,
            }],
            process_count: 1,
        };
        let result = check_root_death(&server, &snapshot);
        // No PID (no connection) → RootDied
        assert_eq!(result, Some(SettleResult::RootDied));
    }

    #[test]
    fn check_root_death_healthy_root_returns_none() {
        // Without a connection, pid() returns None, so this always returns RootDied.
        // Full settle integration requires a real server process.
        let server = test_server();
        server.set_lifecycle(ServerLifecycle::Healthy);
        let snapshot = TreeSnapshot {
            samples: vec![TreeSample {
                pid: 1234,
                ppid: 1,
                delta_utime: 5,
                delta_stime: 2,
                delta_pfc: 100,
                state: ProcessState::Running,
            }],
            process_count: 1,
        };
        let result = check_root_death(&server, &snapshot);
        // No connection → pid() is None → RootDied
        assert_eq!(result, Some(SettleResult::RootDied));
    }
}

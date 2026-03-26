// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Settle sampling loop: polls a process tree at a fixed interval and
//! yields per-process samples to a caller-provided sink.
//!
//! Production code — [`LspClient`](crate::lsp::client::LspClient) will call this in Phase 1b
//! alongside a decision-making sink. The profiling test
//! (`tests/profile_intensity.rs`) uses the same loop with a recording sink.

use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::server::LspServer;

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

/// Run the settle sampling loop.
///
/// Polls `tree_monitor` every `interval`, reads `in_progress_count` from
/// `server`, and calls `sink.record()` for each per-process sample.
/// Runs until `sink.record()` returns `false` or the `cancel` token fires.
///
/// Emits a sample for every process in the tree on every interval,
/// including idle processes with all-zero deltas. The caller needs
/// explicit idle samples to detect settling.
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

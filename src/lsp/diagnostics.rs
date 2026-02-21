// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Diagnostics strategy selection and activity monitoring.
//!
//! After the LSP `initialize` handshake, each server is assigned a
//! [`DiagnosticsStrategy`] based on its advertised capabilities. The
//! strategy determines how Catenary obtains fresh diagnostics after a
//! file change — ranging from pull diagnostics (cleanest) to CPU-time
//! polling (last resort).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// Strategy for waiting until the server has finished processing a
/// change and published fresh diagnostics.
///
/// Ordered by preference — earlier variants provide stronger signals
/// and are preferred when the server exhibits the required behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticsStrategy {
    /// Wait for `publishDiagnostics` with `version >= N`.
    /// Causality via document version.
    Version,
    /// Wait for the server to go Active → Idle via `$/progress` tokens.
    TokenMonitor,
    /// Sample the server process's CPU time to infer activity.
    /// Trust-based timeout with decay.
    ProcessMonitor,
}

/// Activity state reported by a [`ProgressMonitor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityState {
    /// Server is actively working — keep waiting.
    Active,
    /// Server is idle — safe to return cached diagnostics.
    Idle,
    /// Server process died.
    Dead,
}

/// Polls server activity to determine when diagnostics are ready.
///
/// Used by [`DiagnosticsStrategy::PushTokenMonitor`] and
/// [`DiagnosticsStrategy::PushProcessMonitor`] to detect when the server
/// has finished processing a change.
pub trait ProgressMonitor {
    /// Sample the server's current activity state.
    fn poll(&mut self) -> ActivityState;
}

/// Monitors server activity via `$/progress` token state.
///
/// Reads the server's state atomic to determine whether any progress
/// tokens are active. No timeout — the signal is authoritative.
pub struct TokenMonitor {
    /// Server state atomic (0=Initializing, 1=Indexing, 2=Ready, 3=Dead).
    state: Arc<std::sync::atomic::AtomicU8>,
    /// Whether the server process is alive.
    alive: Arc<std::sync::atomic::AtomicBool>,
}

impl TokenMonitor {
    /// Creates a new `TokenMonitor` from the server's shared state.
    pub const fn new(
        state: Arc<std::sync::atomic::AtomicU8>,
        alive: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self { state, alive }
    }
}

impl ProgressMonitor for TokenMonitor {
    fn poll(&mut self) -> ActivityState {
        if !self.alive.load(Ordering::SeqCst) {
            return ActivityState::Dead;
        }

        let state = crate::lsp::state::ServerState::from_u8(self.state.load(Ordering::SeqCst));
        match state {
            crate::lsp::state::ServerState::Indexing => ActivityState::Active,
            crate::lsp::state::ServerState::Dead => ActivityState::Dead,
            // Ready or Initializing — treat as idle for diagnostics purposes
            _ => ActivityState::Idle,
        }
    }
}

/// Monitors server activity via CPU time sampling.
///
/// Reads the server process's CPU time (user + system) and compares
/// with the previous sample. The server is considered active when CPU
/// ticks advance between samples, and idle when flat.
pub struct ProcessMonitor {
    /// PID of the server process.
    pid: u32,
    /// Whether the server process is alive.
    alive: Arc<std::sync::atomic::AtomicBool>,
    /// Previous CPU time sample (ticks).
    last_cpu: Option<u64>,
    /// Reference to the trust counter on `LspClient`.
    trust_failures: Arc<AtomicU32>,
}

impl ProcessMonitor {
    /// Creates a new `ProcessMonitor`.
    pub const fn new(
        pid: u32,
        alive: Arc<std::sync::atomic::AtomicBool>,
        trust_failures: Arc<AtomicU32>,
    ) -> Self {
        Self {
            pid,
            alive,
            last_cpu: None,
            trust_failures,
        }
    }

    /// Returns the maximum patience duration based on the trust counter.
    ///
    /// Each consecutive CPU-path timeout without diagnostics halves the
    /// patience. After 3+ failures, only a 5-second settle is allowed.
    pub fn patience(&self) -> std::time::Duration {
        match self.trust_failures.load(Ordering::SeqCst) {
            0 => std::time::Duration::from_secs(120),
            1 => std::time::Duration::from_secs(60),
            2 => std::time::Duration::from_secs(30),
            _ => std::time::Duration::from_secs(5),
        }
    }
}

impl ProgressMonitor for ProcessMonitor {
    fn poll(&mut self) -> ActivityState {
        if !self.alive.load(Ordering::SeqCst) {
            return ActivityState::Dead;
        }

        let Some(current) = process_cpu_ticks(self.pid) else {
            // Can't read CPU time — process likely dead
            return ActivityState::Dead;
        };

        let result = self.last_cpu.map_or(
            // First sample — assume active (no baseline yet)
            ActivityState::Active,
            |prev| {
                if current > prev {
                    ActivityState::Active
                } else {
                    ActivityState::Idle
                }
            },
        );

        self.last_cpu = Some(current);
        result
    }
}

/// Reads the total CPU time (user + system) for a process, in
/// platform-specific tick units.
///
/// Returns `None` if the process cannot be found or the data cannot be
/// read.
#[cfg(target_os = "linux")]
fn process_cpu_ticks(pid: u32) -> Option<u64> {
    // /proc/<pid>/stat fields: ... (13) utime (14) stime ...
    // Fields are 1-indexed in documentation but 0-indexed in the split.
    // utime = index 13, stime = index 14
    let path = format!("/proc/{pid}/stat");
    let contents = std::fs::read_to_string(path).ok()?;

    // The comm field (index 1) may contain spaces and parentheses, so
    // find the last ')' to skip past it.
    let after_comm = contents.rfind(')')? + 1;
    let fields: Vec<&str> = contents[after_comm..].split_whitespace().collect();

    // After ')' and the state field, utime is at offset 11 and stime at 12
    // (comm-relative: state=0, ppid=1, ... utime=11, stime=12)
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;

    Some(utime + stime)
}

#[cfg(target_os = "macos")]
fn process_cpu_ticks(pid: u32) -> Option<u64> {
    // unsafe is forbidden — shell out to `ps` for CPU time
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "cputime="])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let time_str = String::from_utf8_lossy(&output.stdout);
    parse_cputime_string(time_str.trim())
}

#[cfg(target_os = "macos")]
fn parse_cputime_string(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        2 => {
            // MM:SS.ss
            let minutes: u64 = parts[0].parse().ok()?;
            let seconds: f64 = parts[1].parse().ok()?;
            // Convert to centiseconds for consistent tick units
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "CPU time is always positive and fits in u64"
            )]
            Some(minutes * 6000 + (seconds * 100.0) as u64)
        }
        3 => {
            // HH:MM:SS
            let hours: u64 = parts[0].parse().ok()?;
            let minutes: u64 = parts[1].parse().ok()?;
            let seconds: f64 = parts[2].parse().ok()?;
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "CPU time is always positive and fits in u64"
            )]
            Some(hours * 360_000 + minutes * 6000 + (seconds * 100.0) as u64)
        }
        _ => None,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_cpu_ticks(_pid: u32) -> Option<u64> {
    // Unsupported platform — ProcessMonitor will always report Dead.
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU8};

    #[test]
    fn token_monitor_idle_when_ready() {
        let state = Arc::new(AtomicU8::new(crate::lsp::state::ServerState::Ready.as_u8()));
        let alive = Arc::new(AtomicBool::new(true));
        let mut monitor = TokenMonitor::new(state, alive);

        assert_eq!(monitor.poll(), ActivityState::Idle);
    }

    #[test]
    fn token_monitor_active_when_indexing() {
        let state = Arc::new(AtomicU8::new(
            crate::lsp::state::ServerState::Indexing.as_u8(),
        ));
        let alive = Arc::new(AtomicBool::new(true));
        let mut monitor = TokenMonitor::new(state, alive);

        assert_eq!(monitor.poll(), ActivityState::Active);
    }

    #[test]
    fn token_monitor_dead_when_not_alive() {
        let state = Arc::new(AtomicU8::new(crate::lsp::state::ServerState::Ready.as_u8()));
        let alive = Arc::new(AtomicBool::new(false));
        let mut monitor = TokenMonitor::new(state, alive);

        assert_eq!(monitor.poll(), ActivityState::Dead);
    }

    #[test]
    fn process_monitor_patience_decay() {
        let alive = Arc::new(AtomicBool::new(true));
        let trust = Arc::new(AtomicU32::new(0));
        let monitor = ProcessMonitor::new(1, alive, trust.clone());

        assert_eq!(monitor.patience(), std::time::Duration::from_secs(120));

        trust.store(1, Ordering::SeqCst);
        assert_eq!(monitor.patience(), std::time::Duration::from_secs(60));

        trust.store(2, Ordering::SeqCst);
        assert_eq!(monitor.patience(), std::time::Duration::from_secs(30));

        trust.store(3, Ordering::SeqCst);
        assert_eq!(monitor.patience(), std::time::Duration::from_secs(5));

        trust.store(10, Ordering::SeqCst);
        assert_eq!(monitor.patience(), std::time::Duration::from_secs(5));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_cpu_ticks_self() {
        // Read our own process's CPU time — should always succeed
        let pid = std::process::id();
        let ticks = process_cpu_ticks(pid);
        assert!(ticks.is_some(), "Should be able to read own CPU time");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_cpu_ticks_nonexistent() {
        // PID max shouldn't exist
        let ticks = process_cpu_ticks(u32::MAX);
        assert!(ticks.is_none(), "Nonexistent PID should return None");
    }
}

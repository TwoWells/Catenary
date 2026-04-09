// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Diagnostics strategy selection and activity monitoring.
//!
//! After the LSP `initialize` handshake, each server is assigned a
//! `DiagnosticsStrategy` based on its observed runtime behavior.
//! The strategy determines how Catenary obtains fresh diagnostics
//! after a file change.
//!
//! Servers that provide neither `version` in `publishDiagnostics` nor
//! `$/progress` tokens do not participate in diagnostics — they still
//! receive `didOpen`/`didChange` for code intelligence, but Catenary
//! does not send `didSave` or wait for diagnostics from them.

use std::sync::Arc;

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
    /// Wait for the server to go Active -> Idle via `$/progress` tokens.
    TokenMonitor,
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
/// Used by [`DiagnosticsStrategy::TokenMonitor`] to detect when the
/// server has finished processing a change.
pub trait ProgressMonitor {
    /// Sample the server's current activity state.
    fn poll(&mut self) -> ActivityState;
}

/// Monitors server activity via lifecycle state.
///
/// Reads the server's lifecycle mutex to determine whether the server
/// is busy. No timeout — the signal is authoritative.
///
/// Note: Old wait machinery — deleted in 1b-08.
pub struct TokenMonitor {
    /// Server lifecycle mutex.
    lifecycle: Arc<std::sync::Mutex<crate::lsp::state::ServerLifecycle>>,
    /// Whether the server process is alive.
    alive: Arc<std::sync::atomic::AtomicBool>,
}

impl TokenMonitor {
    /// Creates a new `TokenMonitor` from the server's shared state.
    pub const fn new(
        lifecycle: Arc<std::sync::Mutex<crate::lsp::state::ServerLifecycle>>,
        alive: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self { lifecycle, alive }
    }
}

impl ProgressMonitor for TokenMonitor {
    fn poll(&mut self) -> ActivityState {
        use std::sync::atomic::Ordering;

        if !self.alive.load(Ordering::SeqCst) {
            return ActivityState::Dead;
        }

        let lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match *lifecycle {
            crate::lsp::state::ServerLifecycle::Busy(_) => ActivityState::Active,
            crate::lsp::state::ServerLifecycle::Failed
            | crate::lsp::state::ServerLifecycle::Dead => ActivityState::Dead,
            _ => ActivityState::Idle,
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::lsp::state::ServerLifecycle;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn token_monitor_idle_when_healthy() {
        let lifecycle = Arc::new(std::sync::Mutex::new(ServerLifecycle::Healthy));
        let alive = Arc::new(AtomicBool::new(true));
        let mut monitor = TokenMonitor::new(lifecycle, alive);

        assert_eq!(monitor.poll(), ActivityState::Idle);
    }

    #[test]
    fn token_monitor_active_when_busy() {
        let lifecycle = Arc::new(std::sync::Mutex::new(ServerLifecycle::Busy(1)));
        let alive = Arc::new(AtomicBool::new(true));
        let mut monitor = TokenMonitor::new(lifecycle, alive);

        assert_eq!(monitor.poll(), ActivityState::Active);
    }

    #[test]
    fn token_monitor_idle_when_initializing() {
        let lifecycle = Arc::new(std::sync::Mutex::new(ServerLifecycle::Initializing));
        let alive = Arc::new(AtomicBool::new(true));
        let mut monitor = TokenMonitor::new(lifecycle, alive);

        assert_eq!(monitor.poll(), ActivityState::Idle);
    }

    #[test]
    fn token_monitor_dead_when_not_alive() {
        let lifecycle = Arc::new(std::sync::Mutex::new(ServerLifecycle::Healthy));
        let alive = Arc::new(AtomicBool::new(false));
        let mut monitor = TokenMonitor::new(lifecycle, alive);

        assert_eq!(monitor.poll(), ActivityState::Dead);
    }

    #[test]
    fn token_monitor_dead_when_failed() {
        let lifecycle = Arc::new(std::sync::Mutex::new(ServerLifecycle::Failed));
        let alive = Arc::new(AtomicBool::new(true));
        let mut monitor = TokenMonitor::new(lifecycle, alive);

        assert_eq!(monitor.poll(), ActivityState::Dead);
    }
}

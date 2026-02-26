// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Diagnostics strategy selection and activity monitoring.
//!
//! After the LSP `initialize` handshake, each server is assigned a
//! [`DiagnosticsStrategy`] based on its observed runtime behavior.
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

/// Monitors server activity via `$/progress` token state.
///
/// Reads the server's state atomic to determine whether any progress
/// tokens are active. No timeout — the signal is authoritative.
pub struct TokenMonitor {
    /// Server state atomic (0=Initializing, 1=Busy, 2=Ready, 3=Dead).
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
        use std::sync::atomic::Ordering;

        if !self.alive.load(Ordering::SeqCst) {
            return ActivityState::Dead;
        }

        let state = crate::lsp::state::ServerState::from_u8(self.state.load(Ordering::SeqCst));
        match state {
            crate::lsp::state::ServerState::Busy => ActivityState::Active,
            crate::lsp::state::ServerState::Dead => ActivityState::Dead,
            // Ready or Initializing — treat as idle for diagnostics purposes
            _ => ActivityState::Idle,
        }
    }
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
    fn token_monitor_active_when_busy() {
        let state = Arc::new(AtomicU8::new(crate::lsp::state::ServerState::Busy.as_u8()));
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
}

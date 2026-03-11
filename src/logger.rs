// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Logger trait for capturing protocol events.
//!
//! Tee points call [`Logger::log`] with each protocol message. The
//! implementation decides where it goes (database, tracing, vec).

use crate::session::{EventBroadcaster, EventKind};

/// Trait for capturing protocol events.
///
/// Tee points call `log()` with each protocol message. The
/// implementation decides where it goes (database, tracing, vec).
pub trait Logger: Send + Sync {
    /// Record a protocol event.
    fn log(&self, event: EventKind);
}

/// Production logger — inserts events into `SQLite` via the broadcaster.
pub struct DbLogger {
    broadcaster: EventBroadcaster,
}

impl DbLogger {
    /// Create a new database logger.
    #[must_use]
    pub const fn new(broadcaster: EventBroadcaster) -> Self {
        Self { broadcaster }
    }
}

impl Logger for DbLogger {
    fn log(&self, event: EventKind) {
        self.broadcaster.send(event);
    }
}

/// Debug logger — forwards events to the `tracing` crate.
pub struct TracingLogger;

impl Logger for TracingLogger {
    fn log(&self, event: EventKind) {
        if let EventKind::ProtocolMessage {
            protocol,
            direction,
            message,
            ..
        } = &event
        {
            tracing::trace!(?protocol, ?direction, "{}", message);
        }
    }
}

/// Test logger — collects events into a vec.
pub struct VecLogger {
    events: std::sync::Mutex<Vec<EventKind>>,
}

impl Default for VecLogger {
    fn default() -> Self {
        Self::new()
    }
}

impl VecLogger {
    /// Create a new vec logger.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            events: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Return collected events.
    pub fn events(&self) -> Vec<EventKind> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Logger for VecLogger {
    fn log(&self, event: EventKind) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }
}

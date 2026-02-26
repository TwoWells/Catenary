// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Data abstraction layer for the TUI.
//!
//! [`LiveDataSource`] reads from the filesystem (production).
//! [`MockDataSource`] returns pre-configured data (testing).

use std::collections::HashMap;
use std::collections::VecDeque;

use anyhow::Result;

use crate::session::{self, SessionEvent, SessionInfo, TailReader};

/// Collected session row: info, liveness, and active language servers.
pub struct SessionRow {
    /// Session metadata.
    pub info: SessionInfo,
    /// Whether the session process is still alive.
    pub alive: bool,
    /// Active language server IDs for this session.
    pub languages: Vec<String>,
}

/// Abstraction over session data access.
///
/// [`LiveDataSource`] reads from the filesystem (production).
/// [`MockDataSource`] returns pre-configured data (testing).
pub trait DataSource {
    /// List all sessions with their liveness status and active languages.
    ///
    /// # Errors
    ///
    /// Returns an error if session data cannot be read.
    fn list_sessions(&self) -> Result<Vec<SessionRow>>;

    /// Load all historical events for a session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session does not exist or events cannot be read.
    fn monitor_events(&self, session_id: &str) -> Result<Vec<SessionEvent>>;

    /// Create a tail reader for new events (from current position onward).
    ///
    /// # Errors
    ///
    /// Returns an error if the session does not exist or the tail cannot be created.
    fn create_tail(&self, session_id: &str) -> Result<Box<dyn EventTail>>;

    /// Delete a dead session's data.
    ///
    /// # Errors
    ///
    /// Returns an error if the session directory cannot be removed.
    fn delete_session(&self, session_id: &str) -> Result<()>;
}

/// Tail reader abstraction for streaming new events.
pub trait EventTail {
    /// Read the next event if available. Returns `None` if no new event yet.
    ///
    /// # Errors
    ///
    /// Returns an error if reading from the underlying source fails.
    fn try_next_event(&mut self) -> Result<Option<SessionEvent>>;
}

// ── Live (production) implementation ─────────────────────────────────

/// Data source backed by the real filesystem via [`session`] functions.
pub struct LiveDataSource;

impl DataSource for LiveDataSource {
    fn list_sessions(&self) -> Result<Vec<SessionRow>> {
        let raw = session::list_sessions()?;
        let mut rows: Vec<SessionRow> = raw
            .into_iter()
            .map(|(info, alive)| {
                let languages = session::active_languages(&info.id).unwrap_or_default();
                SessionRow {
                    info,
                    alive,
                    languages,
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            b.alive
                .cmp(&a.alive)
                .then_with(|| b.info.started_at.cmp(&a.info.started_at))
        });
        Ok(rows)
    }

    fn monitor_events(&self, session_id: &str) -> Result<Vec<SessionEvent>> {
        Ok(session::monitor_events(session_id)?.collect())
    }

    fn create_tail(&self, session_id: &str) -> Result<Box<dyn EventTail>> {
        let reader = session::tail_events_new(session_id)?;
        Ok(Box::new(LiveEventTail { reader }))
    }

    fn delete_session(&self, session_id: &str) -> Result<()> {
        let dir = session::sessions_dir().join(session_id);
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}

/// Tail reader backed by a real [`TailReader`].
struct LiveEventTail {
    reader: TailReader,
}

impl EventTail for LiveEventTail {
    fn try_next_event(&mut self) -> Result<Option<SessionEvent>> {
        self.reader.try_next_event()
    }
}

// ── Mock (testing) implementation ────────────────────────────────────

/// Data source backed by in-memory data for deterministic testing.
pub struct MockDataSource {
    /// Sessions to return from [`DataSource::list_sessions`].
    pub sessions: Vec<SessionRow>,
    /// Events keyed by session ID for [`DataSource::monitor_events`].
    pub events: HashMap<String, Vec<SessionEvent>>,
    /// Tail events keyed by session ID for [`DataSource::create_tail`].
    pub tail_events: HashMap<String, VecDeque<SessionEvent>>,
}

impl DataSource for MockDataSource {
    fn list_sessions(&self) -> Result<Vec<SessionRow>> {
        // MockDataSource cannot clone SessionRow (SessionInfo requires Clone,
        // which it derives), so we rebuild rows from the stored data.
        let rows = self
            .sessions
            .iter()
            .map(|r| SessionRow {
                info: r.info.clone(),
                alive: r.alive,
                languages: r.languages.clone(),
            })
            .collect();
        Ok(rows)
    }

    fn monitor_events(&self, session_id: &str) -> Result<Vec<SessionEvent>> {
        self.events
            .get(session_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Session not found: {session_id}"))
    }

    fn create_tail(&self, session_id: &str) -> Result<Box<dyn EventTail>> {
        let events = self
            .tail_events
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        Ok(Box::new(MockEventTail { events }))
    }

    fn delete_session(&self, _session_id: &str) -> Result<()> {
        Ok(())
    }
}

/// Tail reader backed by a [`VecDeque`] for testing.
pub struct MockEventTail {
    events: VecDeque<SessionEvent>,
}

impl MockEventTail {
    /// Create a new mock tail with the given events.
    #[must_use]
    pub const fn new(events: VecDeque<SessionEvent>) -> Self {
        Self { events }
    }
}

impl EventTail for MockEventTail {
    fn try_next_event(&mut self) -> Result<Option<SessionEvent>> {
        Ok(self.events.pop_front())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    use crate::session::{EventKind, SessionInfo};

    fn make_session_info(id: &str) -> SessionInfo {
        SessionInfo {
            id: id.to_string(),
            pid: 1234,
            workspace: "/tmp/test".to_string(),
            started_at: Utc::now(),
            client_name: None,
            client_version: None,
        }
    }

    fn make_event(kind: EventKind) -> SessionEvent {
        SessionEvent {
            timestamp: Utc::now(),
            kind,
        }
    }

    #[test]
    fn test_mock_data_source_list_sessions() -> Result<()> {
        let ds = MockDataSource {
            sessions: vec![
                SessionRow {
                    info: make_session_info("active-1"),
                    alive: true,
                    languages: vec!["rust".to_string()],
                },
                SessionRow {
                    info: make_session_info("dead-1"),
                    alive: false,
                    languages: vec![],
                },
            ],
            events: HashMap::new(),
            tail_events: HashMap::new(),
        };

        let rows = ds.list_sessions()?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].info.id, "active-1");
        assert!(rows[0].alive);
        assert_eq!(rows[0].languages, vec!["rust".to_string()]);
        assert_eq!(rows[1].info.id, "dead-1");
        assert!(!rows[1].alive);
        Ok(())
    }

    #[test]
    fn test_mock_data_source_monitor_events() -> Result<()> {
        let events = vec![
            make_event(EventKind::Started),
            make_event(EventKind::Shutdown),
            make_event(EventKind::Started),
        ];
        let mut event_map = HashMap::new();
        event_map.insert("abc".to_string(), events);

        let ds = MockDataSource {
            sessions: vec![],
            events: event_map,
            tail_events: HashMap::new(),
        };

        let result = ds.monitor_events("abc")?;
        assert_eq!(result.len(), 3);

        let err = ds.monitor_events("nonexistent");
        assert!(err.is_err());
        Ok(())
    }

    #[test]
    fn test_mock_event_tail_drains() -> Result<()> {
        let mut events = VecDeque::new();
        events.push_back(make_event(EventKind::Started));
        events.push_back(make_event(EventKind::Shutdown));

        let mut tail = MockEventTail::new(events);

        assert!(tail.try_next_event()?.is_some());
        assert!(tail.try_next_event()?.is_some());
        assert!(tail.try_next_event()?.is_none());
        Ok(())
    }
}

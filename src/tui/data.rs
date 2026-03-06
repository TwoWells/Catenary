// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Data abstraction layer for the TUI.
//!
//! [`SqliteDataSource`] reads from the database (production).
//! [`MockDataSource`] returns pre-configured data (testing).

use std::collections::HashMap;
use std::collections::VecDeque;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::session::{self, EventKind, SessionEvent, SessionInfo, SqliteEventTail};

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
/// [`SqliteDataSource`] reads from the database (production).
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
    /// Returns an error if the session data cannot be removed.
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

impl EventTail for SqliteEventTail {
    fn try_next_event(&mut self) -> Result<Option<SessionEvent>> {
        self.try_next_event()
    }
}

// ── SQLite (production) implementation ───────────────────────────────

/// Data source backed by `SQLite` via the [`crate::db`] module.
pub struct SqliteDataSource {
    conn: rusqlite::Connection,
}

impl SqliteDataSource {
    /// Open a new data source with a database connection.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or migrated.
    pub fn new() -> Result<Self> {
        let conn = crate::db::open_and_migrate()?;
        Ok(Self { conn })
    }

    /// Create a data source with an existing database connection.
    ///
    /// Useful for testing with isolated temporary databases.
    #[must_use]
    pub const fn with_conn(conn: rusqlite::Connection) -> Self {
        Self { conn }
    }
}

/// Raw row from the sessions table (avoids complex tuple types).
struct RawSessionRow {
    id: String,
    pid: u32,
    display_name: String,
    client_name: Option<String>,
    client_version: Option<String>,
    started_at_str: String,
    db_alive: bool,
}

impl DataSource for SqliteDataSource {
    fn list_sessions(&self) -> Result<Vec<SessionRow>> {
        let raw = {
            let mut stmt = self.conn.prepare(
                "SELECT id, pid, display_name, client_name, client_version, started_at, alive \
                 FROM sessions ORDER BY alive DESC, started_at DESC",
            )?;
            let mut r = stmt.query([])?;
            let mut rows = Vec::new();
            while let Some(row) = r.next()? {
                rows.push(RawSessionRow {
                    id: row.get(0)?,
                    pid: row.get(1)?,
                    display_name: row.get(2)?,
                    client_name: row.get(3)?,
                    client_version: row.get(4)?,
                    started_at_str: row.get(5)?,
                    db_alive: row.get(6)?,
                });
            }
            rows
        };

        let mut sessions = Vec::with_capacity(raw.len());
        for RawSessionRow {
            id,
            pid,
            display_name,
            client_name,
            client_version,
            started_at_str,
            db_alive,
        } in raw
        {
            let started_at = DateTime::parse_from_rfc3339(&started_at_str)
                .with_context(|| format!("invalid started_at: {started_at_str}"))?
                .with_timezone(&Utc);

            let alive = if db_alive {
                if session::is_process_alive(pid) {
                    true
                } else {
                    let _ = self.conn.execute(
                        "UPDATE sessions SET alive = 0, ended_at = ?1 WHERE id = ?2",
                        rusqlite::params![Utc::now().to_rfc3339(), &id],
                    );
                    false
                }
            } else {
                false
            };

            let languages = active_languages_for(&self.conn, &id);

            sessions.push(SessionRow {
                info: SessionInfo {
                    id,
                    pid,
                    workspace: display_name,
                    started_at,
                    client_name,
                    client_version,
                },
                alive,
                languages,
            });
        }

        Ok(sessions)
    }

    fn monitor_events(&self, session_id: &str) -> Result<Vec<SessionEvent>> {
        let mut stmt = self
            .conn
            .prepare("SELECT timestamp, payload FROM events WHERE session_id = ?1 ORDER BY id")?;
        let mut rows = stmt.query([session_id])?;
        let mut events = Vec::new();

        while let Some(row) = rows.next()? {
            let ts: String = row.get(0)?;
            let payload: String = row.get(1)?;

            if let Ok(timestamp) = DateTime::parse_from_rfc3339(&ts)
                && let Ok(kind) = serde_json::from_str::<EventKind>(&payload)
            {
                events.push(SessionEvent {
                    timestamp: timestamp.with_timezone(&Utc),
                    kind,
                });
            }
        }

        Ok(events)
    }

    fn create_tail(&self, session_id: &str) -> Result<Box<dyn EventTail>> {
        let tail = session::tail_events_new(session_id)?;
        Ok(Box::new(tail))
    }

    fn delete_session(&self, session_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", [session_id])?;

        // Clean up socket directory if it exists.
        let socket_dir = session::sessions_dir().join(session_id);
        let _ = std::fs::remove_dir_all(&socket_dir);

        Ok(())
    }
}

/// Query active languages for a session from its events.
fn active_languages_for(conn: &rusqlite::Connection, session_id: &str) -> Vec<String> {
    let mut languages = HashMap::new();

    let Ok(mut stmt) = conn.prepare(
        "SELECT payload FROM events WHERE session_id = ?1 AND kind = 'server_state' ORDER BY id",
    ) else {
        return vec![];
    };

    let Ok(mut rows) = stmt.query([session_id]) else {
        return vec![];
    };

    while let Ok(Some(row)) = rows.next() {
        let Ok(payload) = row.get::<_, String>(0) else {
            continue;
        };
        if let Ok(EventKind::ServerState { language, state }) =
            serde_json::from_str::<EventKind>(&payload)
        {
            if state == "Dead" {
                languages.remove(&language);
            } else {
                languages.insert(language, state);
            }
        }
    }

    let mut result: Vec<String> = languages.into_keys().collect();
    result.sort();
    result
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
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use chrono::Utc;

    use crate::session::{EventKind, SessionInfo};

    /// Open an isolated test database in a tempdir.
    /// Returns `(TempDir, PathBuf, Connection)` — the tempdir guard must
    /// be held for the lifetime of the connection.
    fn test_db() -> (tempfile::TempDir, std::path::PathBuf, rusqlite::Connection) {
        let dir = tempfile::tempdir().expect("failed to create tempdir for test DB");
        let path = dir.path().join("catenary").join("catenary.db");
        let conn = crate::db::open_and_migrate_at(&path).expect("failed to open test DB");
        (dir, path, conn)
    }

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

    // ── Mock tests (unchanged) ───────────────────────────────────────

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

    // ── SQLite tests ─────────────────────────────────────────────────

    /// Create a session backed by the database at `db_path`.
    fn create_session(
        db_path: &std::path::Path,
        workspace: &str,
    ) -> Result<crate::session::Session> {
        let arc = std::sync::Arc::new(std::sync::Mutex::new(crate::db::open_and_migrate_at(
            db_path,
        )?));
        crate::session::Session::create_with_conn(workspace, arc)
    }

    #[test]
    fn test_sqlite_data_source_list_sessions() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let ds = SqliteDataSource::with_conn(conn);

        // Need a separate conn for creating the session (different handle).
        let session = create_session(&path, "/tmp/test-ds-list")?;
        let id = session.info.id.clone();

        let rows = ds.list_sessions()?;
        assert!(rows.iter().any(|r| r.info.id == id));

        drop(session);
        ds.delete_session(&id)?;
        Ok(())
    }

    #[test]
    fn test_sqlite_data_source_monitor_events() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let ds = SqliteDataSource::with_conn(conn);

        let session = create_session(&path, "/tmp/test-ds-events")?;
        let id = session.info.id.clone();

        session.broadcast(EventKind::ServerState {
            language: "rust".to_string(),
            state: "Ready".to_string(),
        });

        let events = ds.monitor_events(&id)?;
        // At least Started + ServerState
        assert!(events.len() >= 2);

        drop(session);
        ds.delete_session(&id)?;
        Ok(())
    }

    #[test]
    fn test_sqlite_event_tail_streams() -> Result<()> {
        let (_dir, path, conn) = test_db();

        let session = create_session(&path, "/tmp/test-ds-tail")?;
        let id = session.info.id.clone();

        // Open a fresh connection for the tail (it takes ownership).
        let tail_conn = crate::db::open_at(&path)?;
        let mut tail = crate::session::tail_events_new_with_conn(tail_conn, &id)?;

        // No new events since tail was created after the Started event
        // (tail_events_new starts from the current end).
        assert!(
            tail.try_next_event()?.is_none(),
            "should have no events initially"
        );

        // Broadcast a new event
        session.broadcast(EventKind::ServerState {
            language: "rust".to_string(),
            state: "Ready".to_string(),
        });

        let event = tail.try_next_event()?;
        assert!(event.is_some(), "should see newly broadcast event");

        // No more events
        assert!(tail.try_next_event()?.is_none());

        drop(session);
        conn.execute("DELETE FROM sessions WHERE id = ?1", [&id])?;
        Ok(())
    }

    #[test]
    fn test_sqlite_data_source_delete_session() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let ds = SqliteDataSource::with_conn(conn);

        let session = create_session(&path, "/tmp/test-ds-delete")?;
        let id = session.info.id.clone();
        drop(session);

        // Should exist
        assert!(ds.list_sessions()?.iter().any(|r| r.info.id == id));

        // Delete
        ds.delete_session(&id)?;

        // Should be gone
        assert!(!ds.list_sessions()?.iter().any(|r| r.info.id == id));

        Ok(())
    }
}

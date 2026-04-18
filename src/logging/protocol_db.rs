// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Protocol message DB sink.
//!
//! Writes events with `kind` in `{"lsp", "mcp", "hook"}` to the `messages`
//! table and broadcasts the inserted ROWID so `SqliteMessageTail` stays live.
//! Sole protocol message writer — `MessageLog` has been removed.

use std::sync::Arc;
use std::sync::Mutex;

use chrono::SecondsFormat;
use chrono::Utc;
use rusqlite::Connection;
use tokio::sync::broadcast;

use super::LogEvent;
use super::Sink;

/// Writes protocol events (`kind` in `{lsp, mcp, hook}`) to the `messages`
/// table and broadcasts the inserted ROWID.
///
/// Non-protocol events (missing or unrecognised `kind`) are silently ignored.
/// DB failures are logged at `trace!` level (not `warn!`, to avoid re-entrant
/// event storms) and the write is dropped. Lock poisoning is recovered via
/// `into_inner` so the sink keeps working after a panic elsewhere.
pub struct ProtocolDbSink {
    conn: Arc<Mutex<Connection>>,
    instance_id: Arc<str>,
    broadcast: broadcast::Sender<i64>,
}

impl ProtocolDbSink {
    /// Create a new protocol DB sink.
    #[must_use]
    pub fn new(conn: Arc<Mutex<Connection>>, instance_id: Arc<str>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(256);
        Arc::new(Self {
            conn,
            instance_id,
            broadcast: tx,
        })
    }

    /// Subscribe to ROWID broadcasts (for testing).
    #[cfg(test)]
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<i64> {
        self.broadcast.subscribe()
    }
}

impl Sink for ProtocolDbSink {
    fn handle(&self, event: &LogEvent<'_>) {
        let Some(kind) = event.kind.as_deref() else {
            return;
        };
        if !matches!(kind, "lsp" | "mcp" | "hook") {
            return;
        }

        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let payload = event.payload.as_deref().unwrap_or("");

        let conn = match self.conn.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::trace!("protocol_db: mutex poisoned, recovering");
                poisoned.into_inner()
            }
        };

        let insert_result = conn.execute(
            "INSERT INTO messages \
             (session_id, timestamp, type, method, server, client, \
              request_id, parent_id, payload) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                &*self.instance_id,
                timestamp,
                kind,
                event.method.as_deref().unwrap_or(""),
                event.server.as_deref().unwrap_or(""),
                event.client.as_deref().unwrap_or(""),
                event.request_id,
                event.parent_id,
                payload,
            ],
        );

        match insert_result {
            Ok(_) => {
                let rowid = conn.last_insert_rowid();
                drop(conn); // release DB lock before broadcasting
                let _ = self.broadcast.send(rowid);
            }
            Err(e) => {
                drop(conn);
                // trace!, not warn!, to avoid re-entrant event storm.
                tracing::trace!(error = %e, "protocol_db: insert failed");
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for assertions")]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use rusqlite::Connection;

    use super::ProtocolDbSink;
    use crate::logging::LogEvent;
    use crate::logging::Severity;
    use crate::logging::Sink;

    /// Create an in-memory DB with the messages table schema.
    fn test_db() -> Arc<Mutex<Connection>> {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            "CREATE TABLE messages (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id  TEXT NOT NULL,
                 timestamp   TEXT NOT NULL,
                 type        TEXT NOT NULL,
                 method      TEXT NOT NULL,
                 server      TEXT NOT NULL,
                 client      TEXT NOT NULL,
                 request_id  INTEGER,
                 parent_id   INTEGER,
                 payload     TEXT NOT NULL
             );",
        )
        .expect("create messages table");
        Arc::new(Mutex::new(conn))
    }

    fn make_event(
        kind: Option<&str>,
        method: Option<&str>,
        server: Option<&str>,
        request_id: Option<i64>,
        parent_id: Option<i64>,
        payload: Option<&str>,
    ) -> LogEvent<'static> {
        LogEvent {
            severity: Severity::Info,
            target: "test",
            message: String::new(),
            kind: kind.map(str::to_string),
            method: method.map(str::to_string),
            server: server.map(str::to_string),
            client: None,
            request_id,
            parent_id,
            source: None,
            language: None,
            payload: payload.map(str::to_string),
            fields: serde_json::Map::new(),
        }
    }

    struct Row {
        session_id: String,
        r#type: String,
        method: String,
        server: String,
        request_id: Option<i64>,
        parent_id: Option<i64>,
        payload: String,
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "MutexGuard must outlive the prepared statement"
    )]
    fn read_rows(conn: &Arc<Mutex<Connection>>) -> Vec<Row> {
        let conn = conn.lock().expect("lock db");
        let mut stmt = conn
            .prepare(
                "SELECT session_id, type, method, server, \
                 request_id, parent_id, payload FROM messages ORDER BY id",
            )
            .expect("prepare select");
        stmt.query_map([], |row| {
            Ok(Row {
                session_id: row.get(0)?,
                r#type: row.get(1)?,
                method: row.get(2)?,
                server: row.get(3)?,
                request_id: row.get(4)?,
                parent_id: row.get(5)?,
                payload: row.get(6)?,
            })
        })
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect rows")
    }

    fn row_count(conn: &Arc<Mutex<Connection>>) -> usize {
        read_rows(conn).len()
    }

    #[test]
    fn writes_lsp_event_to_messages_table() {
        let db = test_db();
        let sink = ProtocolDbSink::new(db.clone(), "sess-1".into());
        let event = make_event(
            Some("lsp"),
            Some("textDocument/hover"),
            Some("rs"),
            None,
            None,
            Some("{}"),
        );
        sink.handle(&event);

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.session_id, "sess-1");
        assert_eq!(r.r#type, "lsp");
        assert_eq!(r.method, "textDocument/hover");
        assert_eq!(r.server, "rs");
        assert_eq!(r.payload, "{}");
    }

    #[test]
    fn writes_mcp_event() {
        let db = test_db();
        let sink = ProtocolDbSink::new(db.clone(), "sess-1".into());
        sink.handle(&make_event(
            Some("mcp"),
            Some("tools/call"),
            None,
            None,
            None,
            Some("{\"tool\":\"grep\"}"),
        ));

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].r#type, "mcp");
        assert_eq!(rows[0].method, "tools/call");
    }

    #[test]
    fn writes_hook_event() {
        let db = test_db();
        let sink = ProtocolDbSink::new(db.clone(), "sess-1".into());
        sink.handle(&make_event(
            Some("hook"),
            Some("post-tool"),
            None,
            None,
            None,
            None,
        ));

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].r#type, "hook");
    }

    #[test]
    fn non_protocol_event_ignored() {
        let db = test_db();
        let sink = ProtocolDbSink::new(db.clone(), "sess-1".into());
        sink.handle(&make_event(None, None, None, None, None, None));
        assert_eq!(row_count(&db), 0);
    }

    #[test]
    fn unknown_kind_ignored() {
        let db = test_db();
        let sink = ProtocolDbSink::new(db.clone(), "sess-1".into());
        sink.handle(&make_event(Some("catenary"), None, None, None, None, None));
        assert_eq!(row_count(&db), 0);
    }

    #[test]
    fn correlation_ids_stored() {
        let db = test_db();
        let sink = ProtocolDbSink::new(db.clone(), "sess-1".into());
        sink.handle(&make_event(
            Some("lsp"),
            Some("hover"),
            None,
            Some(42),
            Some(7),
            None,
        ));

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, Some(42));
        assert_eq!(rows[0].parent_id, Some(7));
    }

    #[test]
    fn nullable_correlation_ids() {
        let db = test_db();
        let sink = ProtocolDbSink::new(db.clone(), "sess-1".into());
        sink.handle(&make_event(
            Some("lsp"),
            Some("hover"),
            None,
            None,
            None,
            None,
        ));

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, None);
        assert_eq!(rows[0].parent_id, None);
    }

    #[test]
    fn db_failure_does_not_panic() {
        let db = test_db();
        let sink = ProtocolDbSink::new(db.clone(), "sess-1".into());

        // Drop the table to force an insert failure.
        db.lock()
            .expect("lock db")
            .execute_batch("DROP TABLE messages")
            .expect("drop table");

        // Should not panic — silently drops the write.
        sink.handle(&make_event(
            Some("lsp"),
            Some("hover"),
            None,
            None,
            None,
            None,
        ));
    }

    #[test]
    fn broadcasts_rowid_on_successful_insert() {
        let db = test_db();
        let sink = ProtocolDbSink::new(db, "sess-1".into());
        let mut rx = sink.subscribe();

        sink.handle(&make_event(
            Some("lsp"),
            Some("hover"),
            None,
            None,
            None,
            None,
        ));

        let rowid = rx.try_recv().expect("should receive rowid");
        assert!(rowid > 0);
    }

    #[test]
    fn broadcast_skipped_on_insert_failure() {
        let db = test_db();
        let sink = ProtocolDbSink::new(db.clone(), "sess-1".into());
        let mut rx = sink.subscribe();

        db.lock()
            .expect("lock db")
            .execute_batch("DROP TABLE messages")
            .expect("drop table");

        sink.handle(&make_event(
            Some("lsp"),
            Some("hover"),
            None,
            None,
            None,
            None,
        ));

        assert!(rx.try_recv().is_err(), "no broadcast on insert failure");
    }
}

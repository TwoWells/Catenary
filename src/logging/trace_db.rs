// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Trace DB sink.
//!
//! Captures non-protocol events (events without `kind`, or `kind` outside
//! `{"lsp", "mcp", "hook"}`) to the `messages` table. Replaces `ErrorLayer`'s
//! writer path. Severity-unfiltered: captures Debug through Error so the trace
//! DB is a complete forensic record.

use std::sync::Arc;
use std::sync::Mutex;

use chrono::SecondsFormat;
use chrono::Utc;
use rusqlite::Connection;
use serde_json::Value;

use super::LogEvent;
use super::Sink;

/// Writes non-protocol trace events to the `messages` table.
///
/// Protocol events (`kind` in `{lsp, mcp, hook}`) are silently skipped — those
/// go to [`super::protocol_db::ProtocolDbSink`]. All severity levels are
/// captured so the trace DB serves as a complete forensic record.
///
/// DB failures are logged at `trace!` level (not `warn!`, to avoid re-entrant
/// event storms) and the write is dropped. Lock poisoning is recovered via
/// `into_inner` so the sink keeps working after a panic elsewhere.
pub struct TraceDbSink {
    conn: Arc<Mutex<Connection>>,
    instance_id: String,
}

impl TraceDbSink {
    /// Create a new trace DB sink.
    #[must_use]
    pub fn new(conn: Arc<Mutex<Connection>>, instance_id: String) -> Arc<Self> {
        Arc::new(Self { conn, instance_id })
    }
}

impl Sink for TraceDbSink {
    fn handle(&self, event: &LogEvent<'_>) {
        // Skip protocol events — those go to ProtocolDbSink.
        if matches!(event.kind.as_deref(), Some("lsp" | "mcp" | "hook")) {
            return;
        }

        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let type_str = match event.severity {
            super::Severity::Error => "error",
            super::Severity::Warn => "warn",
            super::Severity::Info => "info",
            super::Severity::Debug => "debug",
        };
        let payload = build_payload(event);

        let conn = match self.conn.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::trace!("trace_db: mutex poisoned, recovering");
                poisoned.into_inner()
            }
        };

        if let Err(e) = conn.execute(
            "INSERT INTO messages \
             (session_id, timestamp, type, method, server, client, \
              request_id, parent_id, payload) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                self.instance_id,
                timestamp,
                type_str,
                event.target, // `method` column gets the tracing target
                event.server.as_deref().unwrap_or(""),
                "catenary", // `client` column fixed
                event.request_id,
                event.parent_id,
                payload,
            ],
        ) {
            drop(conn);
            // trace!, not warn!, to avoid re-entrant event storm.
            tracing::trace!(error = %e, "trace_db: insert failed");
        }
    }
}

/// Build a JSON payload from a trace event.
///
/// Preserves the existing `ErrorLayer` row shape: `level`, `message`, optional
/// `source`, `language`, and `fields`.
fn build_payload(event: &LogEvent<'_>) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("level".into(), event.severity.tag().into());
    obj.insert("message".into(), event.message.clone().into());
    if let Some(source) = &event.source {
        obj.insert("source".into(), source.clone().into());
    }
    if let Some(language) = &event.language {
        obj.insert("language".into(), language.clone().into());
    }
    if !event.fields.is_empty() {
        obj.insert("fields".into(), Value::Object(event.fields.clone()));
    }
    Value::Object(obj).to_string()
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for assertions")]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use rusqlite::Connection;

    use super::TraceDbSink;
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

    /// Minimal trace event with defaults. Tests override fields directly.
    fn base_event<'a>(severity: Severity, target: &'a str, message: &str) -> LogEvent<'a> {
        LogEvent {
            severity,
            target,
            message: message.to_string(),
            kind: None,
            method: None,
            server: None,
            client: None,
            request_id: None,
            parent_id: None,
            source: None,
            language: None,
            payload: None,
            fields: serde_json::Map::new(),
        }
    }

    struct Row {
        session_id: String,
        r#type: String,
        method: String,
        server: String,
        client: String,
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
                "SELECT session_id, type, method, server, client, \
                 request_id, parent_id, payload FROM messages ORDER BY id",
            )
            .expect("prepare select");
        stmt.query_map([], |row| {
            Ok(Row {
                session_id: row.get(0)?,
                r#type: row.get(1)?,
                method: row.get(2)?,
                server: row.get(3)?,
                client: row.get(4)?,
                request_id: row.get(5)?,
                parent_id: row.get(6)?,
                payload: row.get(7)?,
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
    fn writes_warn_event_with_target() {
        let db = test_db();
        let sink = TraceDbSink::new(db.clone(), "sess-1".into());
        let mut event = base_event(
            Severity::Warn,
            "crate::logging::tests",
            "something happened",
        );
        event.server = Some("rust-analyzer".into());
        event.source = Some("lsp.lifecycle".into());
        sink.handle(&event);

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.session_id, "sess-1");
        assert_eq!(r.r#type, "warn");
        assert_eq!(r.method, "crate::logging::tests");
        assert_eq!(r.server, "rust-analyzer");
        assert_eq!(r.client, "catenary");

        let parsed: serde_json::Value =
            serde_json::from_str(&r.payload).expect("parse payload JSON");
        assert_eq!(parsed["level"], "warn");
        assert_eq!(parsed["message"], "something happened");
        assert_eq!(parsed["source"], "lsp.lifecycle");
    }

    #[test]
    fn skips_protocol_events() {
        let db = test_db();
        let sink = TraceDbSink::new(db.clone(), "sess-1".into());

        for kind in &["lsp", "mcp", "hook"] {
            let mut event = base_event(Severity::Info, "test", "protocol msg");
            event.kind = Some((*kind).to_string());
            sink.handle(&event);
        }

        assert_eq!(row_count(&db), 0);
    }

    #[test]
    fn captures_debug_events() {
        let db = test_db();
        let sink = TraceDbSink::new(db.clone(), "sess-1".into());
        sink.handle(&base_event(Severity::Debug, "test", "dev trace"));

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].r#type, "debug");

        let parsed: serde_json::Value =
            serde_json::from_str(&rows[0].payload).expect("parse payload JSON");
        assert_eq!(parsed["level"], "debug");
        assert_eq!(parsed["message"], "dev trace");
    }

    #[test]
    fn payload_includes_fields() {
        let db = test_db();
        let sink = TraceDbSink::new(db.clone(), "sess-1".into());

        let mut event = base_event(Severity::Warn, "test", "with fields");
        event
            .fields
            .insert("foo".into(), serde_json::Value::String("bar".into()));
        event
            .fields
            .insert("count".into(), serde_json::Value::Number(42.into()));
        sink.handle(&event);

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        let parsed: serde_json::Value =
            serde_json::from_str(&rows[0].payload).expect("parse payload JSON");
        assert_eq!(parsed["fields"]["foo"], "bar");
        assert_eq!(parsed["fields"]["count"], 42);
    }

    #[test]
    fn payload_includes_source_and_language() {
        let db = test_db();
        let sink = TraceDbSink::new(db.clone(), "sess-1".into());
        let mut event = base_event(Severity::Info, "test", "structured");
        event.source = Some("lsp.lifecycle".into());
        event.language = Some("rust".into());
        sink.handle(&event);

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        let parsed: serde_json::Value =
            serde_json::from_str(&rows[0].payload).expect("parse payload JSON");
        assert_eq!(parsed["source"], "lsp.lifecycle");
        assert_eq!(parsed["language"], "rust");
    }

    #[test]
    fn correlation_ids_stored() {
        let db = test_db();
        let sink = TraceDbSink::new(db.clone(), "sess-1".into());
        let mut event = base_event(Severity::Error, "test", "correlated");
        event.request_id = Some(42);
        event.parent_id = Some(7);
        sink.handle(&event);

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, Some(42));
        assert_eq!(rows[0].parent_id, Some(7));
    }

    #[test]
    fn db_failure_does_not_panic() {
        let db = test_db();
        let sink = TraceDbSink::new(db.clone(), "sess-1".into());

        // Drop the table to force an insert failure.
        db.lock()
            .expect("lock db")
            .execute_batch("DROP TABLE messages")
            .expect("drop table");

        // Should not panic — silently drops the write.
        sink.handle(&base_event(Severity::Error, "test", "should not panic"));
    }
}

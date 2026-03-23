// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Tracing layer that captures `error!()` and `warn!()` events to SQLite.
//!
//! Writes to the `messages` table via [`MessageLog`], using `type = "error"`
//! or `type = "warn"`. Events are queryable with `catenary query --kind error`.
//!
//! # Usage
//!
//! ```ignore
//! let (error_layer, handle) = ErrorLayer::new();
//! // ... build subscriber with error_layer ...
//! // After session creation:
//! handle.activate(message_log);
//! ```

use std::sync::{Arc, OnceLock};

use crate::session::MessageLog;

/// Tracing layer that captures `ERROR` and `WARN` events to the database.
///
/// Created with [`ErrorLayer::new`], which returns a handle for deferred
/// activation. The layer is a no-op until activated with a [`MessageLog`].
pub struct ErrorLayer {
    log: Arc<OnceLock<Arc<MessageLog>>>,
}

/// Handle for activating an [`ErrorLayer`] after session creation.
pub struct ErrorLayerHandle {
    log: Arc<OnceLock<Arc<MessageLog>>>,
}

impl ErrorLayer {
    /// Create a new error layer and its activation handle.
    ///
    /// The layer drops events until [`ErrorLayerHandle::activate`] is called.
    #[must_use]
    pub fn new() -> (Self, ErrorLayerHandle) {
        let log = Arc::new(OnceLock::new());
        (Self { log: log.clone() }, ErrorLayerHandle { log })
    }
}

impl ErrorLayerHandle {
    /// Activate the error layer with a message log.
    ///
    /// After this call, `ERROR` and `WARN` events are written to the database.
    pub fn activate(&self, message_log: Arc<MessageLog>) {
        let _ = self.log.set(message_log);
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ErrorLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let Some(log) = self.log.get() else {
            return;
        };

        let meta = event.metadata();
        let level = *meta.level();

        let type_str = if level == tracing::Level::ERROR {
            "error"
        } else if level == tracing::Level::WARN {
            "warn"
        } else {
            return;
        };

        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        let payload = if visitor.fields.is_empty() {
            serde_json::json!({
                "level": level.as_str(),
                "message": visitor.message,
            })
        } else {
            serde_json::json!({
                "level": level.as_str(),
                "message": visitor.message,
                "fields": visitor.fields,
            })
        };

        log.log(
            type_str,
            meta.target(),
            "catenary",
            "",
            None,
            None,
            &payload,
        );
    }
}

/// Visitor that extracts the message and structured fields from a tracing event.
#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields.insert(
                field.name().to_string(),
                serde_json::Value::String(format!("{value:?}")),
            );
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.insert(
                field.name().to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), serde_json::Value::Bool(value));
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tracing_subscriber::layer::SubscriberExt;

    /// Set up a test DB with an activated error layer. Returns the subscriber,
    /// a read connection for assertions, and the tempdir (must be held alive).
    fn setup() -> (
        impl tracing::Subscriber,
        rusqlite::Connection,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.db");
        let conn = crate::db::open_and_migrate_at(&path).expect("open db");

        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at, alive) \
             VALUES ('test-session', 1, 'test', '2026-01-01T00:00:00Z', 1)",
            [],
        )
        .expect("insert session");

        let conn = Arc::new(Mutex::new(conn));
        let message_log = Arc::new(MessageLog::new(conn, "test-session".to_string()));

        let (layer, handle) = ErrorLayer::new();
        handle.activate(message_log);

        let subscriber = tracing_subscriber::registry().with(layer);
        let read_conn = crate::db::open_at(&path).expect("read conn");

        (subscriber, read_conn, dir)
    }

    fn count_messages(conn: &rusqlite::Connection, type_filter: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE type = ?1",
            [type_filter],
            |row| row.get(0),
        )
        .expect("count query")
    }

    #[test]
    fn test_error_layer_captures_error() {
        let (subscriber, conn, _dir) = setup();
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!("something went wrong");
        });
        assert_eq!(count_messages(&conn, "error"), 1);
    }

    #[test]
    fn test_error_layer_skips_info() {
        let (subscriber, conn, _dir) = setup();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("informational message");
        });
        assert_eq!(count_messages(&conn, "error"), 0);
        assert_eq!(count_messages(&conn, "warn"), 0);
    }

    #[test]
    fn test_error_layer_captures_warn() {
        let (subscriber, conn, _dir) = setup();
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!("a warning occurred");
        });
        assert_eq!(count_messages(&conn, "warn"), 1);
    }

    #[test]
    fn test_error_layer_payload_structure() {
        let (subscriber, conn, _dir) = setup();
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(code = 42, "fetch failed");
        });

        let payload: String = conn
            .query_row(
                "SELECT payload FROM messages WHERE type = 'error' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("query payload");

        let parsed: serde_json::Value = serde_json::from_str(&payload).expect("parse payload JSON");
        assert_eq!(parsed["level"], "ERROR");
        assert_eq!(parsed["message"], "fetch failed");
        assert_eq!(parsed["fields"]["code"], 42);
    }

    #[test]
    fn test_error_layer_inactive_before_activate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.db");
        let conn = crate::db::open_and_migrate_at(&path).expect("open db");

        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at, alive) \
             VALUES ('test-session', 1, 'test', '2026-01-01T00:00:00Z', 1)",
            [],
        )
        .expect("insert session");

        // Create layer but do NOT activate it
        let (layer, _handle) = ErrorLayer::new();
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::error!("this should be dropped");
        });

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .expect("count query");
        assert_eq!(count, 0, "no messages should be written before activation");
    }

    #[test]
    fn test_error_query_kind_filter() {
        let (subscriber, conn, _dir) = setup();
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!("an error");
            tracing::warn!("a warning");
        });

        // Simulate what `catenary query --kind error` does
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages m WHERE m.type = 'error'",
                [],
                |row| row.get(0),
            )
            .expect("count query");
        assert_eq!(count, 1, "--kind error should find exactly 1 error");

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages m WHERE m.type = 'warn'",
                [],
                |row| row.get(0),
            )
            .expect("count query");
        assert_eq!(count, 1, "--kind warn should find exactly 1 warning");
    }
}

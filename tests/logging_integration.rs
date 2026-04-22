// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Cross-cutting integration tests for `LoggingServer`.
//!
//! Tests in this file exercise multi-sink dispatch, notification queue
//! threshold/dedup, and protocol message round-trip through the full
//! tracing Layer pipeline — scenarios that span multiple tickets and
//! don't fit naturally inside a single module's test suite.
//!
//! Each test uses `tracing::subscriber::with_default` (scoped per-test)
//! to avoid global subscriber conflicts in parallel test execution.

use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use tempfile::tempdir;
use tracing_subscriber::layer::SubscriberExt;

use catenary_mcp::logging::notification_queue::NotificationQueueSink;
use catenary_mcp::logging::protocol_db::ProtocolDbSink;
use catenary_mcp::logging::trace_db::TraceDbSink;
use catenary_mcp::logging::{LoggingServer, Severity};

const MOCK_LANG_A: &str = "yX4Za";

// ── Helpers ────────────────────────────────────────────────────────────

/// Create an in-memory DB with the `messages` table schema.
fn test_db() -> Arc<Mutex<rusqlite::Connection>> {
    let conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
    conn.execute_batch(
        "CREATE TABLE sessions (
             id           TEXT PRIMARY KEY,
             pid          INTEGER NOT NULL,
             display_name TEXT NOT NULL,
             started_at   TEXT NOT NULL
         );
         INSERT INTO sessions (id, pid, display_name, started_at)
             VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z');
         CREATE TABLE messages (
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
    .expect("create schema");
    Arc::new(Mutex::new(conn))
}

/// Row projection from the `messages` table.
struct MsgRow {
    r#type: String,
    method: String,
    request_id: Option<i64>,
    parent_id: Option<i64>,
}

/// Query all messages ordered by id.
#[allow(
    clippy::significant_drop_tightening,
    reason = "MutexGuard must outlive the prepared statement"
)]
fn query_messages(conn: &Arc<Mutex<rusqlite::Connection>>) -> Vec<MsgRow> {
    let c = conn.lock().expect("lock");
    c.prepare("SELECT type, method, request_id, parent_id FROM messages ORDER BY id")
        .expect("prepare")
        .query_map([], |row| {
            Ok(MsgRow {
                r#type: row.get(0)?,
                method: row.get(1)?,
                request_id: row.get(2)?,
                parent_id: row.get(3)?,
            })
        })
        .expect("query")
        .filter_map(std::result::Result::ok)
        .collect()
}

/// Count rows in the `messages` table.
fn message_count(conn: &Arc<Mutex<rusqlite::Connection>>) -> usize {
    query_messages(conn).len()
}

// ── Multi-sink dispatch ────────────────────────────────────────────────

/// Verify that all three sinks (notification queue, protocol DB, trace DB)
/// receive their respective events through a single `LoggingServer` Layer.
#[test]
fn multi_sink_dispatch_routes_correctly() {
    let db = test_db();
    let notifications = NotificationQueueSink::new(Severity::Warn);
    let protocol_db = ProtocolDbSink::new(db.clone(), "s1".into());
    let trace_db = TraceDbSink::new(db.clone(), "s1".into());

    let server = LoggingServer::new();
    let subscriber = tracing_subscriber::registry().with(server.clone());
    tracing::subscriber::with_default(subscriber, || {
        server.activate(vec![notifications.clone(), protocol_db, trace_db]);

        // Protocol event (kind="lsp") → protocol DB only, not trace DB.
        tracing::info!(
            kind = "lsp",
            method = "textDocument/hover",
            server = "rust-analyzer",
            payload = "{}",
            "outgoing"
        );

        // Warn event without kind → trace DB + notification queue.
        tracing::warn!(source = "lsp.lifecycle", "server crashed");

        // Debug event without kind → trace DB only (below notification threshold).
        tracing::debug!("verbose trace");
    });

    let msgs = query_messages(&db);

    // Protocol DB: 1 lsp event. Trace DB: 2 events (warn + debug).
    // Total DB rows: 3.
    assert_eq!(msgs.len(), 3, "expected 3 DB rows, got {}", msgs.len());

    // Protocol event is type "lsp".
    assert_eq!(msgs[0].r#type, "lsp");
    assert_eq!(msgs[0].method, "textDocument/hover");

    // Trace events are type "warn" and "debug".
    assert_eq!(msgs[1].r#type, "warn");
    assert_eq!(msgs[2].r#type, "debug");

    // Notification queue: 1 warn event.
    let drained = notifications.drain();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].message, "server crashed");
}

/// Verify that notification queue threshold filtering works end-to-end
/// through the tracing Layer.
#[test]
fn notification_threshold_filters_through_layer() {
    let notifications = NotificationQueueSink::new(Severity::Warn);
    let server = LoggingServer::new();

    let subscriber = tracing_subscriber::registry().with(server.clone());
    tracing::subscriber::with_default(subscriber, || {
        server.activate(vec![notifications.clone()]);

        tracing::debug!("below threshold");
        tracing::info!("below threshold");
        tracing::warn!(server = "a", "at threshold");
        tracing::error!(server = "b", "above threshold");
    });

    let drained = notifications.drain();
    assert_eq!(drained.len(), 2, "only warn + error should enqueue");
    assert_eq!(drained[0].severity, Severity::Warn);
    assert_eq!(drained[1].severity, Severity::Error);
}

/// Verify dedup works through the Layer — identical messages dedup,
/// different servers do not.
#[test]
fn notification_dedup_through_layer() {
    let notifications = NotificationQueueSink::new(Severity::Warn);
    let server = LoggingServer::new();

    let subscriber = tracing_subscriber::registry().with(server.clone());
    tracing::subscriber::with_default(subscriber, || {
        server.activate(vec![notifications.clone()]);

        tracing::warn!(server = "ra", "server offline");
        tracing::warn!(server = "ra", "server offline"); // dedup
        tracing::warn!(server = "pylsp", "server offline"); // different server
    });

    let drained = notifications.drain();
    assert_eq!(
        drained.len(),
        2,
        "identical message with same server should dedup"
    );
}

// ── Protocol message round-trip ────────────────────────────────────────

/// Spawn mockls, fire an LSP request, and verify the full scope chain:
/// MCP tool call's correlation ID appears as the LSP request's `parent_id`.
#[tokio::test]
async fn lsp_request_scope_chain() -> Result<()> {
    let db = test_db();
    let protocol_db = ProtocolDbSink::new(db.clone(), "s1".into());
    let server = LoggingServer::new();
    server.activate(vec![protocol_db]);

    let subscriber = tracing_subscriber::registry().with(server.clone());
    let guard = tracing::subscriber::set_default(subscriber);

    let dir = tempdir()?;
    let bin = env!("CARGO_BIN_EXE_mockls");
    let mut client = catenary_mcp::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A],
        MOCK_LANG_A,
        MOCK_LANG_A,
        server.clone(),
        None,
        std::collections::HashMap::new(),
    )?;
    client.initialize(&[dir.path().to_path_buf()], None).await?;

    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "let MY_VAR\n")?;
    let uri = format!("file://{}", file.display());
    client
        .did_open(&uri, MOCK_LANG_A, 1, "let MY_VAR\n")
        .await?;

    // Simulate an MCP tool call context by setting parent_id.
    let mcp_correlation = server.next_id();
    client.set_parent_id(Some(mcp_correlation.0));

    let _hover = client.hover(&uri, 0, 4).await?;

    let msgs = query_messages(&db);
    let hover_msgs: Vec<&MsgRow> = msgs
        .iter()
        .filter(|m| m.method == "textDocument/hover")
        .collect();

    assert!(
        hover_msgs.len() >= 2,
        "expected request + response, got {}",
        hover_msgs.len()
    );

    // Request carries the MCP parent_id.
    assert_eq!(
        hover_msgs[0].parent_id,
        Some(mcp_correlation.0),
        "request parent_id should be the MCP correlation ID"
    );

    // Both carry the same request_id (pair-merge key).
    assert!(hover_msgs[0].request_id.is_some());
    assert_eq!(
        hover_msgs[0].request_id, hover_msgs[1].request_id,
        "request and response should share request_id"
    );

    drop(guard);
    Ok(())
}

/// Verify that `pair_merge` semantics are preserved: querying by
/// `request_id` returns both the request and response rows.
#[tokio::test]
async fn pair_merge_still_works() -> Result<()> {
    let db = test_db();
    let protocol_db = ProtocolDbSink::new(db.clone(), "s1".into());
    let server = LoggingServer::new();
    server.activate(vec![protocol_db]);

    let subscriber = tracing_subscriber::registry().with(server.clone());
    let guard = tracing::subscriber::set_default(subscriber);

    let dir = tempdir()?;
    let bin = env!("CARGO_BIN_EXE_mockls");
    let mut client = catenary_mcp::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A],
        MOCK_LANG_A,
        MOCK_LANG_A,
        server.clone(),
        None,
        std::collections::HashMap::new(),
    )?;
    client.initialize(&[dir.path().to_path_buf()], None).await?;

    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "let MY_VAR\n")?;
    let uri = format!("file://{}", file.display());
    client
        .did_open(&uri, MOCK_LANG_A, 1, "let MY_VAR\n")
        .await?;

    let _def = client.definition(&uri, 0, 4).await?;

    // Find the definition request's correlation ID.
    let msgs = query_messages(&db);
    let def_msgs: Vec<&MsgRow> = msgs
        .iter()
        .filter(|m| m.method == "textDocument/definition")
        .collect();

    assert!(def_msgs.len() >= 2, "expected at least request + response");

    let corr_id = def_msgs[0]
        .request_id
        .expect("request should have request_id");

    // Query by request_id — should return both request and response.
    let pair_count = {
        let c = db.lock().expect("lock");
        c.query_row(
            "SELECT COUNT(*) FROM messages WHERE request_id = ?1",
            [corr_id],
            |row| row.get::<_, i64>(0),
        )
        .expect("count pair")
    };

    assert_eq!(
        pair_count, 2,
        "pair-merge: request_id {corr_id} should match exactly 2 rows"
    );

    drop(guard);
    Ok(())
}

/// Verify that trace DB and protocol DB don't overlap: protocol events
/// go only to protocol DB, non-protocol events go only to trace DB.
#[test]
fn sink_routing_no_overlap() {
    let db = test_db();
    let protocol_db = ProtocolDbSink::new(db.clone(), "s1".into());
    let trace_db = TraceDbSink::new(db.clone(), "s1".into());
    let server = LoggingServer::new();

    let subscriber = tracing_subscriber::registry().with(server.clone());
    tracing::subscriber::with_default(subscriber, || {
        server.activate(vec![protocol_db, trace_db]);

        // 3 protocol events.
        for kind in &["lsp", "mcp", "hook"] {
            tracing::info!(kind = *kind, method = "test", payload = "{}", "protocol");
        }

        // 2 non-protocol events.
        tracing::warn!("trace event 1");
        tracing::info!("trace event 2");
    });

    let msgs = query_messages(&db);

    let protocol_count = msgs
        .iter()
        .filter(|m| matches!(m.r#type.as_str(), "lsp" | "mcp" | "hook"))
        .count();
    let trace_count = msgs
        .iter()
        .filter(|m| matches!(m.r#type.as_str(), "warn" | "info"))
        .count();

    assert_eq!(protocol_count, 3, "3 protocol events");
    assert_eq!(trace_count, 2, "2 trace events");
    assert_eq!(msgs.len(), 5, "no overlap or duplication");
}

/// Verify that `LoggingServer::buffered_len` reports correctly during
/// the bootstrap phase, and that all buffered events are drained to
/// sinks on activation.
#[test]
fn bootstrap_buffer_drains_to_all_sinks() {
    let db = test_db();
    let notifications = NotificationQueueSink::new(Severity::Warn);
    let trace_db = TraceDbSink::new(db.clone(), "s1".into());
    let server = LoggingServer::new();

    let subscriber = tracing_subscriber::registry().with(server.clone());
    tracing::subscriber::with_default(subscriber, || {
        // Bootstrap: events buffered.
        tracing::warn!(source = "config.parse", "bad TOML");
        tracing::warn!(source = "config.parse", server = "x", "bad key");
        assert_eq!(server.buffered_len(), 2);

        // Activate: buffer drains to sinks.
        server.activate(vec![notifications.clone(), trace_db]);
        assert_eq!(server.buffered_len(), 0);
    });

    // Trace DB got both events.
    assert_eq!(message_count(&db), 2);

    // Notification queue got both (both are warn, distinct keys).
    let drained = notifications.drain();
    assert_eq!(drained.len(), 2);
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for LSP message logging via `LoggingServer` + `MessageDbSink`.

use anyhow::Result;
use std::sync::Arc;
use tempfile::tempdir;
use tracing_subscriber::layer::SubscriberExt;

use catenary_mcp::logging::LoggingServer;
use catenary_mcp::logging::message_db::MessageDbSink;

const MOCK_LANG_A: &str = "yX4Za";

/// Row from the `messages` table with the fields we care about.
struct MsgRow {
    r#type: String,
    method: String,
    request_id: Option<i64>,
    parent_id: Option<i64>,
}

/// Create a test DB with a `LoggingServer` backed by a `MessageDbSink`,
/// installed as the thread-local tracing subscriber.
///
/// Returns the `LoggingServer` (for `LspClient::spawn`), the DB connection
/// (for querying), and a guard that restores the previous subscriber on drop.
fn setup_logging() -> (
    LoggingServer,
    Arc<std::sync::Mutex<rusqlite::Connection>>,
    tracing::subscriber::DefaultGuard,
) {
    let conn = Arc::new(std::sync::Mutex::new(
        rusqlite::Connection::open_in_memory().expect("open in-memory db"),
    ));
    conn.lock()
        .expect("lock")
        .execute_batch(
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
                 level       TEXT NOT NULL DEFAULT 'info',
                 method      TEXT NOT NULL,
                 server      TEXT NOT NULL,
                 client      TEXT NOT NULL,
                 request_id  INTEGER,
                 parent_id   INTEGER,
                 payload     TEXT NOT NULL
             );",
        )
        .expect("create schema");

    let logging = LoggingServer::new();
    let message_db = MessageDbSink::new(conn.clone(), "s1".into());
    logging.activate(vec![message_db]);

    let subscriber = tracing_subscriber::registry().with(logging.clone());
    let guard = tracing::subscriber::set_default(subscriber);

    (logging, conn, guard)
}

/// Query all messages from the test DB, ordered by id.
fn query_messages(conn: &Arc<std::sync::Mutex<rusqlite::Connection>>) -> Vec<MsgRow> {
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

/// Spawn mockls with a `LoggingServer` and initialize it.
async fn spawn_initialized_client(
    logging: LoggingServer,
) -> Result<(catenary_mcp::lsp::LspClient, tempfile::TempDir)> {
    let dir = tempdir()?;
    let bin = env!("CARGO_BIN_EXE_mockls");

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A],
        MOCK_LANG_A,
        MOCK_LANG_A,
        logging,
        None,
        std::collections::HashMap::new(),
    )?;

    client.initialize(&[dir.path().to_path_buf()], None).await?;
    Ok((client, dir))
}

/// Verify that a hover request/response pair is logged with correct
/// correlation ID linking (both rows share the same `request_id`).
#[tokio::test]
async fn test_lsp_log_request_response() -> Result<()> {
    let (logging, conn, _guard) = setup_logging();
    let (client, dir) = spawn_initialized_client(logging).await?;

    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "let MY_VAR\n")?;
    let uri = format!("file://{}", file.display());

    client
        .did_open(&uri, MOCK_LANG_A, 1, "let MY_VAR\n")
        .await?;
    let _def = client.definition(&uri, 0, 4).await?;

    let msgs = query_messages(&conn);
    let def_msgs: Vec<&MsgRow> = msgs
        .iter()
        .filter(|m| m.method == "textDocument/definition")
        .collect();

    assert!(
        def_msgs.len() >= 2,
        "expected at least 2 definition messages (request + response), got {}",
        def_msgs.len()
    );

    // All should be "lsp" type
    for m in &def_msgs {
        assert_eq!(m.r#type, "lsp");
    }

    // Both request and response carry the same correlation ID as request_id
    let request = def_msgs[0];
    let response = def_msgs[1];
    assert!(
        request.request_id.is_some(),
        "request should have a correlation ID as request_id"
    );
    assert_eq!(
        response.request_id, request.request_id,
        "response request_id should match request (pair-merge via correlation ID)"
    );

    Ok(())
}

/// Verify that an outbound notification is logged.
#[tokio::test]
async fn test_lsp_log_notification() -> Result<()> {
    let (logging, conn, _guard) = setup_logging();
    let (client, dir) = spawn_initialized_client(logging).await?;

    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "let X\n")?;
    let uri = format!("file://{}", file.display());

    client.did_open(&uri, MOCK_LANG_A, 1, "let X\n").await?;

    let msgs = query_messages(&conn);
    let did_open_msgs: Vec<&MsgRow> = msgs
        .iter()
        .filter(|m| m.method == "textDocument/didOpen")
        .collect();

    assert!(
        !did_open_msgs.is_empty(),
        "expected at least 1 didOpen message"
    );
    assert_eq!(did_open_msgs[0].r#type, "lsp");

    Ok(())
}

/// Verify that inbound notifications (e.g., `textDocument/publishDiagnostics`)
/// are logged.
#[tokio::test]
async fn test_lsp_log_inbound_notification() -> Result<()> {
    let (logging, conn, _guard) = setup_logging();
    let (client, dir) = spawn_initialized_client(logging).await?;

    // mockls publishes diagnostics for files with "error" in the content
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "error here\n")?;
    let uri = format!("file://{}", file.display());

    client
        .did_open(&uri, MOCK_LANG_A, 1, "error here\n")
        .await?;

    // Give the server time to publish diagnostics
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let msgs = query_messages(&conn);
    let diag_msgs: Vec<&MsgRow> = msgs
        .iter()
        .filter(|m| m.method == "textDocument/publishDiagnostics")
        .collect();

    assert!(
        !diag_msgs.is_empty(),
        "expected at least 1 publishDiagnostics message"
    );
    assert_eq!(diag_msgs[0].r#type, "lsp");

    Ok(())
}

/// Verify that `set_parent_id` propagates to request and response messages.
#[tokio::test]
async fn test_lsp_log_parent_id() -> Result<()> {
    let (logging, conn, _guard) = setup_logging();
    let (mut client, dir) = spawn_initialized_client(logging).await?;

    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "let MY_VAR\n")?;
    let uri = format!("file://{}", file.display());

    client
        .did_open(&uri, MOCK_LANG_A, 1, "let MY_VAR\n")
        .await?;

    // Use a correlation ID value as parent (simulating an MCP tool call context)
    let parent_id = 42_i64;
    client.set_parent_id(Some(parent_id));

    let _def = client.definition(&uri, 0, 4).await?;

    let msgs = query_messages(&conn);
    let def_msgs: Vec<&MsgRow> = msgs
        .iter()
        .filter(|m| m.method == "textDocument/definition")
        .collect();

    assert!(
        def_msgs.len() >= 2,
        "expected at least 2 definition messages (request + response), got {}",
        def_msgs.len(),
    );

    // Request carries the MCP parent_id
    assert_eq!(def_msgs[0].parent_id, Some(parent_id));
    // Response parent_id is the correlation ID (self-referential pair)
    assert_eq!(def_msgs[1].parent_id, def_msgs[1].request_id);

    Ok(())
}

/// Verify that the response message in DB has `method` annotated from the
/// pending map (not from the raw JSON-RPC response, which has no method).
#[tokio::test]
async fn test_lsp_log_pending_method_annotation() -> Result<()> {
    let (logging, conn, _guard) = setup_logging();
    let (client, dir) = spawn_initialized_client(logging).await?;

    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "let MY_VAR\n")?;
    let uri = format!("file://{}", file.display());

    client
        .did_open(&uri, MOCK_LANG_A, 1, "let MY_VAR\n")
        .await?;
    let _def = client.definition(&uri, 0, 4).await?;

    let msgs = query_messages(&conn);
    let def_msgs: Vec<&MsgRow> = msgs
        .iter()
        .filter(|m| m.method == "textDocument/definition")
        .collect();

    assert!(
        def_msgs.len() >= 2,
        "expected at least 2 definition messages"
    );

    // The response (second message) should have method="textDocument/definition"
    // even though JSON-RPC responses don't carry a method field.
    assert_eq!(
        def_msgs[1].method, "textDocument/definition",
        "response should be annotated with the request method from the pending map"
    );

    Ok(())
}

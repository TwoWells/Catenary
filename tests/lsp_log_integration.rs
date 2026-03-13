// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for LSP message logging via `MessageLog`.

use anyhow::Result;
use std::sync::Arc;
use tempfile::tempdir;

use catenary_mcp::session::MessageLog;

const MOCK_LANG_A: &str = "yX4Za";

/// Row from the `messages` table with the fields we care about.
struct MsgRow {
    id: i64,
    r#type: String,
    method: String,
    request_id: Option<i64>,
    parent_id: Option<i64>,
}

/// Create a test DB and return a `MessageLog` backed by it, plus the
/// connection for querying.
fn test_message_log() -> (
    tempfile::TempDir,
    Arc<MessageLog>,
    Arc<std::sync::Mutex<rusqlite::Connection>>,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("catenary").join("catenary.db");
    let conn = catenary_mcp::db::open_and_migrate_at(&path).expect("open test db");
    conn.execute(
        "INSERT INTO sessions (id, pid, display_name, started_at) \
         VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z')",
        [],
    )
    .expect("insert session");
    let conn = Arc::new(std::sync::Mutex::new(conn));
    let log = Arc::new(MessageLog::new(conn.clone(), "s1".to_string()));
    (dir, log, conn)
}

/// Query all messages from the test DB, ordered by id.
fn query_messages(conn: &Arc<std::sync::Mutex<rusqlite::Connection>>) -> Vec<MsgRow> {
    let c = conn.lock().expect("lock");
    c.prepare("SELECT id, type, method, request_id, parent_id FROM messages ORDER BY id")
        .expect("prepare")
        .query_map([], |row| {
            Ok(MsgRow {
                id: row.get(0)?,
                r#type: row.get(1)?,
                method: row.get(2)?,
                request_id: row.get(3)?,
                parent_id: row.get(4)?,
            })
        })
        .expect("query")
        .filter_map(std::result::Result::ok)
        .collect()
}

/// Spawn mockls with a live `MessageLog` and initialize it.
async fn spawn_initialized_client(
    message_log: Arc<MessageLog>,
) -> Result<(catenary_mcp::lsp::LspClient, tempfile::TempDir)> {
    let dir = tempdir()?;
    let bin = env!("CARGO_BIN_EXE_mockls");

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A],
        MOCK_LANG_A,
        catenary_mcp::session::EventBroadcaster::noop(),
        message_log,
        None,
    )?;

    client.initialize(&[dir.path().to_path_buf()], None).await?;
    Ok((client, dir))
}

/// Verify that a hover request/response pair is logged with correct
/// `request_id` linking.
#[tokio::test]
async fn test_lsp_log_request_response() -> Result<()> {
    let (_db_dir, log, conn) = test_message_log();
    let (client, dir) = spawn_initialized_client(log).await?;

    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "let MY_VAR\n")?;
    let uri = format!("file://{}", file.display());

    client
        .did_open(&uri, MOCK_LANG_A, 1, "let MY_VAR\n")
        .await?;
    let _hover = client.hover(&uri, 0, 4).await?;

    let msgs = query_messages(&conn);
    let hover_msgs: Vec<&MsgRow> = msgs
        .iter()
        .filter(|m| m.method == "textDocument/hover")
        .collect();

    assert!(
        hover_msgs.len() >= 2,
        "expected at least 2 hover messages (request + response), got {}",
        hover_msgs.len()
    );

    // All should be "lsp" type
    for m in &hover_msgs {
        assert_eq!(m.r#type, "lsp");
    }

    // The response should have request_id pointing to the request
    let request = hover_msgs[0];
    let response = hover_msgs[1];
    assert!(
        request.request_id.is_none(),
        "request should have no request_id"
    );
    assert_eq!(
        response.request_id,
        Some(request.id),
        "response request_id should point to the request row"
    );

    Ok(())
}

/// Verify that an outbound notification is logged.
#[tokio::test]
async fn test_lsp_log_notification() -> Result<()> {
    let (_db_dir, log, conn) = test_message_log();
    let (client, dir) = spawn_initialized_client(log).await?;

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
    let (_db_dir, log, conn) = test_message_log();
    let (client, dir) = spawn_initialized_client(log).await?;

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

/// Verify that `set_parent_id` propagates to both request and response
/// messages.
#[tokio::test]
async fn test_lsp_log_parent_id() -> Result<()> {
    // Identical to test_lsp_log_request_response except for set_parent_id
    let (_db_dir, log, conn) = test_message_log();
    let (mut client, dir) = spawn_initialized_client(log).await?;

    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "let MY_VAR\n")?;
    let uri = format!("file://{}", file.display());

    client
        .did_open(&uri, MOCK_LANG_A, 1, "let MY_VAR\n")
        .await?;

    // Insert a synthetic parent message to satisfy the FK constraint
    let parent_id = {
        let c = conn.lock().expect("lock");
        c.execute(
            "INSERT INTO messages (session_id, timestamp, type, method, server, client, payload) \
             VALUES ('s1', '2026-01-01T00:00:00Z', 'mcp', 'tools/call', 'catenary', 'claude', '{}')",
            [],
        )
        .expect("insert parent");
        c.last_insert_rowid()
    };

    client.set_parent_id(Some(parent_id));

    let _hover = client.hover(&uri, 0, 4).await?;

    let msgs = query_messages(&conn);
    let hover_msgs: Vec<&MsgRow> = msgs
        .iter()
        .filter(|m| m.method == "textDocument/hover")
        .collect();

    assert!(
        hover_msgs.len() >= 2,
        "expected at least 2 hover messages (request + response), got {}. all: {:?}",
        hover_msgs.len(),
        msgs.iter()
            .map(|m| format!(
                "{}(id={},req={:?},par={:?})",
                m.method, m.id, m.request_id, m.parent_id
            ))
            .collect::<Vec<_>>()
    );

    // Both request and response should carry the parent_id
    assert_eq!(hover_msgs[0].parent_id, Some(parent_id));
    assert_eq!(hover_msgs[1].parent_id, Some(parent_id));

    Ok(())
}

/// Verify that the response message in DB has `method` annotated from the
/// pending map (not from the raw JSON-RPC response, which has no method).
#[tokio::test]
async fn test_lsp_log_pending_method_annotation() -> Result<()> {
    let (_db_dir, log, conn) = test_message_log();
    let (client, dir) = spawn_initialized_client(log).await?;

    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "let MY_VAR\n")?;
    let uri = format!("file://{}", file.display());

    client
        .did_open(&uri, MOCK_LANG_A, 1, "let MY_VAR\n")
        .await?;
    let _hover = client.hover(&uri, 0, 4).await?;

    let msgs = query_messages(&conn);
    let hover_msgs: Vec<&MsgRow> = msgs
        .iter()
        .filter(|m| m.method == "textDocument/hover")
        .collect();

    assert!(hover_msgs.len() >= 2, "expected at least 2 hover messages");

    // The response (second message) should have method="textDocument/hover"
    // even though JSON-RPC responses don't carry a method field.
    assert_eq!(
        hover_msgs[1].method, "textDocument/hover",
        "response should be annotated with the request method from the pending map"
    );

    Ok(())
}

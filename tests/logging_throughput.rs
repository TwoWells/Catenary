// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
#![allow(
    clippy::print_stdout,
    reason = "benchmark test prints throughput results"
)]
#![allow(
    clippy::cast_precision_loss,
    reason = "precision loss is acceptable for display-only benchmark stats"
)]
//! Integration benchmark for LSP message throughput via `LoggingServer`.
//!
//! Spawns mockls, fires rounds of hover + definition requests, and measures
//! wall-clock elapsed and DB write throughput (messages/sec). Run with:
//!
//! ```text
//! make test T=logging_throughput
//! ```

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tempfile::tempdir;
use tracing_subscriber::layer::SubscriberExt;

use catenary_mcp::logging::LoggingServer;
use catenary_mcp::logging::protocol_db::ProtocolDbSink;

const MOCK_LANG_A: &str = "yX4Za";

/// Create a test DB with a `LoggingServer` backed by a `ProtocolDbSink`,
/// installed as the thread-local tracing subscriber.
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
    let protocol_db = ProtocolDbSink::new(conn.clone(), "s1".into());
    logging.activate(vec![protocol_db]);

    let subscriber = tracing_subscriber::registry().with(logging.clone());
    let guard = tracing::subscriber::set_default(subscriber);

    (logging, conn, guard)
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
    )?;

    client.initialize(&[dir.path().to_path_buf()], None).await?;
    Ok((client, dir))
}

/// Count total rows in the `messages` table.
fn message_count(conn: &Arc<std::sync::Mutex<rusqlite::Connection>>) -> i64 {
    let c = conn.lock().expect("lock");
    c.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .expect("count messages")
}

/// End-to-end benchmark: hover + definition requests through mockls with
/// `LoggingServer` + `ProtocolDbSink`.
///
/// Prints throughput numbers. Does not assert a hard regression bound
/// because absolute timing varies by machine — the synthetic bench
/// (`benches/logging_overhead.rs`) covers the per-event overhead budget.
#[tokio::test]
async fn bench_lsp_message_throughput() -> Result<()> {
    let (logging, conn, _guard) = setup_logging();
    let (client, dir) = spawn_initialized_client(logging).await?;

    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "let MY_VAR\n")?;
    let uri = format!("file://{}", file.display());
    client
        .did_open(&uri, MOCK_LANG_A, 1, "let MY_VAR\n")
        .await?;

    // Baseline message count (initialize + didOpen).
    let pre_count = message_count(&conn);

    let rounds = 100;
    let start = Instant::now();
    for _ in 0..rounds {
        let _hover = client.hover(&uri, 0, 4).await?;
        let _def = client.definition(&uri, 0, 4).await?;
    }
    let elapsed = start.elapsed();

    let post_count = message_count(&conn);
    let tool_msgs = post_count - pre_count;

    println!();
    println!("LSP message throughput (mockls, in-memory DB)");
    println!("==============================================");
    println!("Rounds:           {rounds} (hover + definition each)");
    println!("Elapsed:          {:.2} ms", elapsed.as_secs_f64() * 1000.0);
    println!("Messages logged:  {tool_msgs}");
    println!(
        "Throughput:       {:.0} msgs/sec",
        tool_msgs as f64 / elapsed.as_secs_f64()
    );
    println!(
        "Per-round:        {:.2} ms",
        elapsed.as_secs_f64() * 1000.0 / f64::from(rounds)
    );

    // Sanity: each request/response pair is 2 messages, 2 methods per round.
    // Minimum expected: rounds × 2 methods × 2 messages = 400.
    assert!(
        tool_msgs >= i64::from(rounds * 2 * 2),
        "expected at least {} messages, got {tool_msgs}",
        rounds * 2 * 2,
    );

    Ok(())
}

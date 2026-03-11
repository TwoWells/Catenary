// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for session event broadcasting and monitoring.
//!
//! These tests exercise the production `Session` → `EventBroadcaster` →
//! `monitor_events` pipeline in-process, verifying that MCP messages
//! broadcast by the server are readable as structured events.

use anyhow::{Context, Result};
use catenary_mcp::db;
use catenary_mcp::session::{self, Direction, EventKind, Protocol, Session};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[test]
fn test_monitor_raw_messages() -> Result<()> {
    let dir = tempfile::tempdir().expect("failed to create tempdir");
    let db_path = dir.path().join("catenary").join("catenary.db");
    let conn = db::open_and_migrate_at(&db_path)?;

    let arc = Arc::new(Mutex::new(db::open_and_migrate_at(&db_path)?));
    let session = Session::create_with_conn("/tmp/monitor-test", arc)?;
    let session_id = session.info.id.clone();
    let broadcaster = session.broadcaster();

    // Broadcast an incoming MCP request
    broadcaster.send(EventKind::ProtocolMessage {
        protocol: Protocol::Mcp,
        language: None,
        direction: Direction::Recv,
        message: json!({
            "jsonrpc": "2.0",
            "id": 12345,
            "method": "ping"
        }),
    });

    // Broadcast an outgoing MCP response
    broadcaster.send(EventKind::ProtocolMessage {
        protocol: Protocol::Mcp,
        language: None,
        direction: Direction::Send,
        message: json!({
            "jsonrpc": "2.0",
            "id": 12345,
            "result": {}
        }),
    });

    // Read events back using the monitor API with the test connection
    let events = session::monitor_events_with_conn(&conn, &session_id)?;

    // Find the incoming ping and outgoing result
    let found_in = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::ProtocolMessage {
                direction: Direction::Recv,
                message,
                ..
            } if message.get("method").and_then(|m: &serde_json::Value| m.as_str()) == Some("ping")
        )
    });

    let found_out = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::ProtocolMessage {
                direction: Direction::Send,
                message,
                ..
            } if message.get("result").is_some()
        )
    });

    // Session cleanup happens via Drop (removes the session directory)
    drop(session);

    assert!(
        found_in,
        "Did not find incoming protocol message (direction=Recv, method=\"ping\") in events"
    );
    assert!(
        found_out,
        "Did not find outgoing protocol message (direction=Send, result) in events"
    );

    // Verify the messages round-tripped with correct content
    let in_event = events
        .iter()
        .find(|e| {
            matches!(
                &e.kind,
                EventKind::ProtocolMessage {
                    direction: Direction::Recv,
                    ..
                }
            )
        })
        .context("incoming event missing")?;

    if let EventKind::ProtocolMessage { message, .. } = &in_event.kind {
        assert_eq!(message["id"], 12345);
        assert_eq!(message["method"], "ping");
    }

    Ok(())
}

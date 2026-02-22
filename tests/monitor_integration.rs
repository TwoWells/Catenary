// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Integration tests for session event broadcasting and monitoring.
//!
//! These tests exercise the production `Session` → `EventBroadcaster` →
//! `monitor_events` pipeline in-process, verifying that MCP messages
//! broadcast by the server are readable as structured events.

use anyhow::{Context, Result};
use catenary_mcp::session::{self, EventKind, Session};
use serde_json::json;

#[test]
fn test_monitor_raw_messages() -> Result<()> {
    // Create a real session (writes events.jsonl to the state directory)
    let session = Session::create("/tmp/monitor-test")?;
    let session_id = session.info.id.clone();
    let broadcaster = session.broadcaster();

    // Broadcast an incoming MCP request
    broadcaster.send(EventKind::McpMessage {
        direction: "in".to_string(),
        message: json!({
            "jsonrpc": "2.0",
            "id": 12345,
            "method": "ping"
        }),
    });

    // Broadcast an outgoing MCP response
    broadcaster.send(EventKind::McpMessage {
        direction: "out".to_string(),
        message: json!({
            "jsonrpc": "2.0",
            "id": 12345,
            "result": {}
        }),
    });

    // Read events back using the production monitor API
    let events: Vec<_> = session::monitor_events(&session_id)?.collect();

    // Find the incoming ping and outgoing result
    let found_in = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::McpMessage {
                direction,
                message,
            } if direction == "in"
                && message.get("method").and_then(|m| m.as_str()) == Some("ping")
        )
    });

    let found_out = events.iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::McpMessage {
                direction,
                message,
            } if direction == "out"
                && message.get("result").is_some()
        )
    });

    // Session cleanup happens via Drop (removes the session directory)
    drop(session);

    assert!(
        found_in,
        "Did not find incoming MCP message (direction=\"in\", method=\"ping\") in events"
    );
    assert!(
        found_out,
        "Did not find outgoing MCP message (direction=\"out\", result) in events"
    );

    // Verify the messages round-tripped with correct content
    let in_event = events
        .iter()
        .find(|e| matches!(&e.kind, EventKind::McpMessage { direction, .. } if direction == "in"))
        .context("incoming event missing")?;

    if let EventKind::McpMessage { message, .. } = &in_event.kind {
        assert_eq!(message["id"], 12345);
        assert_eq!(message["method"], "ping");
    }

    Ok(())
}

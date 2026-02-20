// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Integration test for JSON language server diagnostics timing.
//!
//! Verifies that Catenary's post-warmup grace period correctly catches
//! diagnostics from language servers (like vscode-json-language-server)
//! that only publish diagnostics after a file is opened, not at startup.
//!
//! This reproduces the scenario where:
//! 1. The JSON LSP starts and passes the 10-second warmup without any
//!    files being opened (so `has_published_diagnostics` remains false).
//! 2. The first file is opened after warmup.
//! 3. Without the grace period, `wait_for_diagnostics_update` would
//!    short-circuit and return 0 diagnostics.

use anyhow::{Context, Result};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use tempfile::tempdir;

/// Check if a command exists in PATH.
fn command_exists(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn test_json_diagnostics_on_first_open_past_warmup() -> Result<()> {
    if !command_exists("vscode-json-language-server") {
        tracing::warn!("Skipping test: vscode-json-language-server not installed");
        return Ok(());
    }

    // 1. Create workspace with invalid JSON (trailing comma → parse error)
    let dir = tempdir()?;
    let file_path = dir.path().join("test.json");
    std::fs::write(
        &file_path,
        r#"{
  "name": "test",
}"#,
    )?;

    // 2. Start Catenary with only the JSON LSP
    let mut child = Command::new("cargo")
        .args([
            "run",
            "--",
            "--root",
            dir.path().to_str().context("invalid path")?,
            "--lsp",
            "json:vscode-json-language-server --stdio",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Failed to start catenary")?;

    let stdin = child.stdin.as_mut().context("Failed to get stdin")?;
    let stdout = child.stdout.as_mut().context("Failed to get stdout")?;
    let mut reader = BufReader::new(stdout);

    // 3. Initialize MCP
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1.0" }
        }
    });
    writeln!(stdin, "{init_req}").context("Failed to write init request")?;
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("Failed to read init response")?;

    let initialized_notif = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    writeln!(stdin, "{initialized_notif}").context("Failed to write initialized notification")?;

    // 4. Wait past the warmup period.
    //
    // During this time the JSON LSP is running but has no open files,
    // so it never publishes diagnostics. After warmup expires,
    // has_published_diagnostics is still false.
    std::thread::sleep(catenary_mcp::lsp::WARMUP_PERIOD + std::time::Duration::from_secs(1));

    // 5. Request diagnostics on the invalid JSON file.
    //
    // This is the first file interaction. Without the post-warmup grace
    // period, wait_for_diagnostics_update would short-circuit and return
    // "No diagnostics" because has_published_diagnostics is false.
    let diag_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "diagnostics",
            "arguments": {
                "file": file_path.to_str().context("invalid path")?
            }
        }
    });

    writeln!(stdin, "{diag_req}").context("Failed to write diagnostics request")?;

    line.clear();
    reader
        .read_line(&mut line)
        .context("Failed to read diagnostics response")?;

    // 6. Verify diagnostics were returned.
    //
    // The JSON LSP should report a trailing comma error. The response
    // must NOT be "No diagnostics" — that would mean the grace period
    // failed and we short-circuited past the LSP's response.
    assert!(
        !line.contains("No diagnostics"),
        "JSON diagnostics should be returned on first file open past warmup. Got: {line}"
    );

    // Cleanup
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

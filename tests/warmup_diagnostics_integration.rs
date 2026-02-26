// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration test for diagnostics timing after warmup.
//!
//! Verifies that Catenary's post-warmup grace period correctly catches
//! diagnostics from language servers that only publish diagnostics after a
//! file is opened, not at startup.
//!
//! This reproduces the scenario where:
//! 1. The LSP starts and passes the 10-second warmup without any files
//!    being opened (so `has_published_diagnostics` remains false).
//! 2. The first file is opened after warmup.
//! 3. Without the grace period, `wait_for_diagnostics_update` would
//!    short-circuit and return 0 diagnostics.

use anyhow::{Context, Result};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use tempfile::tempdir;

#[test]
fn test_diagnostics_on_first_open_past_warmup() -> Result<()> {
    // 1. Create workspace with a test file
    let dir = tempdir()?;
    let file_path = dir.path().join("test.sh");
    std::fs::write(&file_path, "#!/bin/bash\necho hello\n")?;

    // 2. Start Catenary with mockls using --publish-version (versioned diagnostics)
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let lsp_arg = format!("shellscript:{mockls_bin} --publish-version");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_catenary"))
        .args([
            "--root",
            dir.path().to_str().context("invalid path")?,
            "--lsp",
            &lsp_arg,
        ])
        .env("XDG_CONFIG_HOME", dir.path())
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
    // During this time the LSP is running but has no open files,
    // so it never publishes diagnostics. After warmup expires,
    // has_published_diagnostics is still false.
    std::thread::sleep(catenary_mcp::lsp::WARMUP_PERIOD + std::time::Duration::from_secs(1));

    // 5. Request diagnostics on the test file.
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

    // 6. Verify the response completed without error or timeout.
    //
    // mockls with --publish-version publishes diagnostics after didOpen,
    // so the grace period should catch them.
    let parsed: serde_json::Value =
        serde_json::from_str(&line).context("Response should be valid JSON")?;
    assert!(
        parsed.get("result").is_some(),
        "Response should contain a result (no RPC error). Got: {line}"
    );

    let result = &parsed["result"];
    let content = result["content"]
        .as_array()
        .context("Missing content array")?;
    let text = content[0]["text"]
        .as_str()
        .context("Missing text in content")?;
    assert!(
        text.contains("mock diagnostic") || text.contains("mockls"),
        "Expected mock diagnostics from mockls, got: {text}"
    );

    // Cleanup
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

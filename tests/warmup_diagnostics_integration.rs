// SPDX-License-Identifier: AGPL-3.0-or-later
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

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tempfile::tempdir;

const MOCK_LANG_A: &str = "yX4Za";

/// Scans the sessions directory for a `notify.sock` file.
fn find_notify_socket(sessions_dir: &std::path::Path) -> Result<PathBuf> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(entries) = std::fs::read_dir(sessions_dir) {
            for entry in entries.flatten() {
                let sock = entry.path().join("notify.sock");
                if sock.exists() {
                    return Ok(sock);
                }
            }
        }
        if std::time::Instant::now() > deadline {
            bail!(
                "No notify.sock found in {} within 5s",
                sessions_dir.display()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn test_diagnostics_on_first_open_past_warmup() -> Result<()> {
    // 1. Create workspace with a test file
    let dir = tempdir()?;
    let state_dir = tempdir()?;
    let file_path = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file_path, "echo hello\n")?;

    // 2. Start Catenary with mockls using --publish-version (versioned diagnostics)
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let lsp_arg = format!("{MOCK_LANG_A}:{mockls_bin} {MOCK_LANG_A} --publish-version");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_catenary"))
        .env("CATENARY_SERVERS", &lsp_arg)
        .env("CATENARY_ROOTS", dir.path())
        .env("XDG_CONFIG_HOME", dir.path())
        .env("XDG_STATE_HOME", state_dir.path())
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
    std::thread::sleep(catenary_mcp::lsp::WARMUP_PERIOD + Duration::from_secs(1));

    // 5. Request diagnostics via the notify socket.
    //
    // This is the first file interaction. Without the post-warmup grace
    // period, wait_for_diagnostics_update would short-circuit and return
    // empty because has_published_diagnostics is false.
    let sessions_dir = state_dir.path().join("catenary").join("sessions");
    let socket_path = find_notify_socket(&sessions_dir)?;

    let mut stream = std::os::unix::net::UnixStream::connect(&socket_path)
        .context("connect to notify socket")?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .context("set read timeout")?;

    let request = json!({"method": "post-tool/diagnostics", "file": file_path.to_str().context("invalid path")?});
    writeln!(stream, "{request}").context("write to notify socket")?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .context("shutdown write")?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .context("read from notify socket")?;

    // 6. Verify the response contains diagnostics.
    //
    // mockls with --publish-version publishes diagnostics after didOpen,
    // so the grace period should catch them.
    let text = response.trim();
    assert!(
        text.contains("mock diagnostic") || text.contains("mockls"),
        "Expected mock diagnostics from mockls, got: {text}"
    );

    // Cleanup
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

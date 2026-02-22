// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Regression test for issue #3: LSP diagnostics timing.
//!
//! Verifies that Catenary correctly waits for the LSP server to complete
//! its analysis after a file change before returning diagnostics,
//! ensuring accuracy and avoiding race conditions.

use anyhow::{Context, Result};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use tempfile::tempdir;

#[test]
fn test_lsp_diagnostics_waits_for_analysis_after_change() -> Result<()> {
    // 1. Setup workspace with a valid rust file
    let dir = tempdir()?;
    let src_dir = dir.path().join("src");
    std::fs::create_dir(&src_dir)?;
    let file_path = src_dir.join("main.rs");
    std::fs::write(&file_path, "fn main() { let x: i32 = 1; }")?;

    std::fs::write(
        dir.path().join("Cargo.toml"),
        r#"
[package]
name = "test-crate"
version = "0.1.0"
"#,
    )?;

    // 2. Start Catenary
    let mut child = Command::new(env!("CARGO_BIN_EXE_catenary"))
        .args([
            "--root",
            dir.path().to_str().context("invalid path")?,
            "--lsp",
            "rust:rust-analyzer",
        ])
        // Isolate from user-level config
        .env("XDG_CONFIG_HOME", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("Failed to start catenary")?;

    // Take ownership of stdin/stdout so we can drop stdin later for graceful shutdown
    let mut stdin = child.stdin.take().context("Failed to get stdin")?;
    let stdout = child.stdout.take().context("Failed to get stdout")?;
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
    writeln!(stdin, "{init_req}").context("Failed to write to stdin")?;
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("Failed to read from stdout")?;

    let initialized_notif = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    writeln!(stdin, "{initialized_notif}").context("Failed to write to stdin")?;

    // Give it a moment to finish initial indexing
    std::thread::sleep(std::time::Duration::from_millis(5000));

    // 4. Update file to introduce error
    std::fs::write(&file_path, "fn main() { let x: i32 = \"string\"; }")?;

    // 5. Call lsp_diagnostics IMMEDIATELY
    let diag_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "diagnostics",
            "arguments": {
                "file": file_path.to_str().context("invalid path")?,
                "wait_for_reanalysis": true
            }
        }
    });

    writeln!(stdin, "{diag_req}").context("Failed to write to stdin")?;

    line.clear();
    reader
        .read_line(&mut line)
        .context("Failed to read from stdout")?;

    // 6. Verify result contains the expected error
    assert!(
        line.contains("mismatched types") || line.contains("expected i32"),
        "Diagnostics should contain the error after change. Got: {line}"
    );

    // Graceful shutdown: drop stdin so the MCP server exits its read loop,
    // which triggers shutdown_all() → LSP shutdown/exit for rust-analyzer.
    // This prevents the leaked process that nextest flags.
    drop(stdin);
    drop(reader);
    let _ = child.wait();

    Ok(())
}

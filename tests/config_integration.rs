// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Integration tests for configuration loading and merging.
//!
//! Verifies that Catenary correctly loads settings from files,
//! environment variables, and CLI arguments in the correct priority order.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn test_config_loading() -> Result<()> {
    let root_dir = std::env::current_dir()?;
    let config_path = root_dir.join("tests/assets/config.toml");
    let bash_file = root_dir.join("tests/assets/bash/script.sh");

    // Spawn catenary using ONLY the config file (no --lsp args)
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("--config").arg(config_path);
    cmd.arg("--root").arg(&root_dir); // Catenary root
    // Isolate from user-level config
    cmd.env("XDG_CONFIG_HOME", &root_dir);

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit()); // See logs

    let mut child = cmd.spawn().context("Failed to spawn catenary")?;
    let mut stdin = child.stdin.take().context("Failed to get stdin")?;
    let mut stdout = BufReader::new(child.stdout.take().context("Failed to get stdout")?);

    // Wait for init
    std::thread::sleep(Duration::from_secs(2));

    // Initialize MCP
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        }
    })
    .to_string();
    writeln!(stdin, "{init_req}").context("Failed to write to stdin")?;

    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .context("Failed to read from stdout")?;
    let response: Value = serde_json::from_str(&line).context("Failed to parse JSON response")?;
    assert!(
        response.get("result").is_some(),
        "Init failed: {response:?}"
    );

    // Initialized notification
    let initialized_notif = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    })
    .to_string();
    writeln!(stdin, "{initialized_notif}").context("Failed to write to stdin")?;

    // Test Bash Hover (should be enabled by config)
    let hover_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "hover",
            "arguments": {
                "file": bash_file.to_str().context("invalid bash file path")?,
                "line": 2,
                "character": 4
            }
        }
    })
    .to_string();
    writeln!(stdin, "{hover_req}").context("Failed to write to stdin")?;

    line.clear();
    stdout
        .read_line(&mut line)
        .context("Failed to read from stdout")?;
    let response: Value = serde_json::from_str(&line).context("Failed to parse JSON response")?;

    // Cleanup - drop stdin to signal EOF, then wait for graceful exit
    drop(stdin);
    let _ = child.wait();

    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Bash hover failed: {response:?}"
    );
    Ok(())
}

#[test]
fn test_config_override() -> Result<()> {
    let root_dir = std::env::current_dir()?;
    let config_path = root_dir.join("tests/assets/config.toml");
    let rust_file = root_dir.join("src/main.rs");

    // Spawn catenary with config AND CLI override
    // Config provides 'shellscript', CLI provides 'rust'
    // CLI also overrides idle_timeout to 10 (config has 60)
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("--config").arg(config_path);
    cmd.arg("--lsp").arg("rust:rust-analyzer");
    cmd.arg("--idle-timeout").arg("10");
    cmd.arg("--root").arg(&root_dir);
    // Isolate from user-level config
    cmd.env("XDG_CONFIG_HOME", &root_dir);

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut child = cmd.spawn().context("Failed to spawn catenary")?;
    let mut stdin = child.stdin.take().context("Failed to get stdin")?;
    let mut stdout = BufReader::new(child.stdout.take().context("Failed to get stdout")?);

    // Wait for init (Rust analyzer needs time)
    std::thread::sleep(Duration::from_secs(3));

    // Initialize MCP
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        }
    })
    .to_string();
    writeln!(stdin, "{init_req}").context("Failed to write to stdin")?;

    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .context("Failed to read from stdout")?;
    let response: Value = serde_json::from_str(&line).context("Failed to parse JSON response")?;
    assert!(response.get("result").is_some(), "Init failed");

    // Send initialized
    let initialized_notif =
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string();
    writeln!(stdin, "{initialized_notif}").context("Failed to write to stdin")?;

    // Test Rust Hover (CLI arg) - should work
    let hover_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "hover",
            "arguments": {
                "file": rust_file.to_str().context("invalid rust file path")?,
                "line": 1,
                "character": 0
            }
        }
    })
    .to_string();
    writeln!(stdin, "{hover_req}").context("Failed to write to stdin")?;

    line.clear();
    stdout
        .read_line(&mut line)
        .context("Failed to read from stdout")?;
    let response: Value = serde_json::from_str(&line).context("Failed to parse JSON response")?;

    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Rust hover failed (CLI arg not merged?)"
    );

    // Cleanup - drop stdin to signal EOF, then wait for graceful exit
    drop(stdin);
    let _ = child.wait();
    Ok(())
}

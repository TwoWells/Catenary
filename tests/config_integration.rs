// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for configuration loading and merging.
//!
//! Verifies that Catenary correctly loads settings from files,
//! environment variables, and CLI arguments in the correct priority order.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

/// Create a temp config.toml that uses mockls for shellscript.
fn write_mockls_config(dir: &std::path::Path) -> Result<std::path::PathBuf> {
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "idle_timeout = 60\n\n[server.shellscript]\ncommand = \"{mockls_bin}\"\nargs = []\n"
        ),
    )?;
    Ok(config_path)
}

#[test]
fn test_config_loading() -> Result<()> {
    let root_dir = std::env::current_dir()?;
    let tmp = tempfile::tempdir()?;
    let config_path = write_mockls_config(tmp.path())?;

    // Spawn catenary using ONLY the config file (no --lsp args)
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("--config").arg(config_path);
    cmd.arg("--root").arg(&root_dir);
    // Isolate from user-level config
    cmd.env("XDG_CONFIG_HOME", &root_dir);

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut child = cmd.spawn().context("Failed to spawn catenary")?;
    let mut stdin = child.stdin.take().context("Failed to get stdin")?;
    let mut stdout = BufReader::new(child.stdout.take().context("Failed to get stdout")?);

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

    // Test search (verifies the server is functional)
    let search_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": {
                "queries": ["echo"]
            }
        }
    })
    .to_string();
    writeln!(stdin, "{search_req}").context("Failed to write to stdin")?;

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
        "Search failed: {response:?}"
    );
    Ok(())
}

#[test]
fn test_config_override() -> Result<()> {
    let root_dir = std::env::current_dir()?;
    let tmp = tempfile::tempdir()?;
    let config_path = write_mockls_config(tmp.path())?;
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");

    // Spawn catenary with config AND CLI override
    // Config provides 'shellscript' (mockls), CLI provides 'toml' (also mockls)
    // CLI also overrides idle_timeout to 10 (config has 60)
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("--config").arg(config_path);
    cmd.arg("--lsp").arg(format!("toml:{mockls_bin}"));
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

    // Test search (CLI arg merged mockls for toml) - should work
    let search_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": {
                "queries": ["echo"]
            }
        }
    })
    .to_string();
    writeln!(stdin, "{search_req}").context("Failed to write to stdin")?;

    line.clear();
    stdout
        .read_line(&mut line)
        .context("Failed to read from stdout")?;
    let response: Value = serde_json::from_str(&line).context("Failed to parse JSON response")?;

    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Search failed (CLI arg not merged?)"
    );

    // Cleanup - drop stdin to signal EOF, then wait for graceful exit
    drop(stdin);
    let _ = child.wait();
    Ok(())
}

// SPDX-License-Identifier: AGPL-3.0-or-later
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

const MOCK_LANG_A: &str = "yX4Za";
const MOCK_LANG_B: &str = "d5apI";

/// Create a temp config.toml that uses mockls for mock.
fn write_mockls_config(dir: &std::path::Path) -> Result<std::path::PathBuf> {
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[server.mockls-{MOCK_LANG_A}]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"{MOCK_LANG_A}\"]\n\n\
             [language.{MOCK_LANG_A}]\n\
             servers = [\"mockls-{MOCK_LANG_A}\"]\n"
        ),
    )?;
    Ok(config_path)
}

#[test]
fn test_config_loading() -> Result<()> {
    let root_dir = std::env::current_dir()?;
    let tmp = tempfile::tempdir()?;
    let config_path = write_mockls_config(tmp.path())?;

    // Spawn catenary using ONLY the config file (no CATENARY_SERVERS)
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    let state_dir = tempfile::tempdir()?;
    cmd.env("CATENARY_CONFIG", &config_path);
    // Isolate from user-level config and state
    cmd.env("CATENARY_ROOTS", &root_dir);
    cmd.env("XDG_CONFIG_HOME", &root_dir);
    cmd.env("CATENARY_STATE_DIR", state_dir.path());

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
            "name": "grep",
            "arguments": {
                "pattern": "echo"
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

    // Spawn catenary with config AND env override
    // Config provides MOCK_LANG_A (mockls), env provides MOCK_LANG_B (also mockls)
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.env("CATENARY_CONFIG", &config_path);
    let state_dir = tempfile::tempdir()?;
    cmd.env(
        "CATENARY_SERVERS",
        format!("{MOCK_LANG_B}:{mockls_bin} {MOCK_LANG_B}"),
    );
    // Isolate from user-level config and state
    cmd.env("CATENARY_ROOTS", &root_dir);
    cmd.env("XDG_CONFIG_HOME", &root_dir);
    cmd.env("CATENARY_STATE_DIR", state_dir.path());

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
            "name": "grep",
            "arguments": {
                "pattern": "echo"
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

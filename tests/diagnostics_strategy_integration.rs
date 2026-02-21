// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Integration tests for the diagnostics strategy redesign.
//!
//! Uses mockls with various flags to exercise each strategy path:
//! - Version matching (`--publish-version`)
//! - Token monitoring (`--progress-on-change`)
//! - Process monitoring (default — no progress, no version)
//! - Server death (`--drop-after`)

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// Helper to spawn the bridge with mockls and communicate via MCP.
struct BridgeProcess {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<BufReader<std::process::ChildStdout>>,
}

impl BridgeProcess {
    fn spawn(mockls_args: &[&str], root: &str) -> Result<Self> {
        let mockls_bin = env!("CARGO_BIN_EXE_mockls");
        let mut lsp_cmd = format!("shellscript:{mockls_bin}");
        for arg in mockls_args {
            lsp_cmd.push(' ');
            lsp_cmd.push_str(arg);
        }

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        cmd.arg("--lsp")
            .arg(&lsp_cmd)
            .arg("--root")
            .arg(root)
            .env("XDG_CONFIG_HOME", root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn().context("Failed to spawn bridge")?;
        let stdin = child.stdin.take().context("Failed to get stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("Failed to get stdout")?);

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
        })
    }

    fn send(&mut self, request: &Value) -> Result<()> {
        let json = serde_json::to_string(request)?;
        let stdin = self.stdin.as_mut().context("Stdin already closed")?;
        writeln!(stdin, "{json}").context("Failed to write to stdin")?;
        stdin.flush().context("Failed to flush stdin")?;
        Ok(())
    }

    fn recv(&mut self) -> Result<Value> {
        let mut line = String::new();
        let stdout = self.stdout.as_mut().context("Stdout already closed")?;
        stdout
            .read_line(&mut line)
            .context("Failed to read from stdout")?;
        serde_json::from_str(&line).context("Failed to parse JSON response")
    }

    fn initialize(&mut self) -> Result<()> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "diag-strategy-test",
                    "version": "1.0.0"
                }
            }
        }))?;

        let response = self.recv()?;
        if response.get("result").is_none() {
            bail!("Initialize failed: {response:?}");
        }

        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))?;

        std::thread::sleep(Duration::from_millis(200));
        Ok(())
    }

    fn call_diagnostics(&mut self, id: u64, file: &str) -> Result<Value> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "diagnostics",
                "arguments": { "file": file }
            }
        }))?;
        self.recv()
    }
}

impl Drop for BridgeProcess {
    fn drop(&mut self) {
        // Close stdin to signal shutdown
        self.stdin.take();
        let _ = self.child.wait();
    }
}

/// Default mockls: publishes diagnostics on didOpen/didChange without
/// version or progress tokens. Exercises the `ProcessMonitor` path.
#[test]
fn test_diagnostics_process_monitor_path() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("test.sh");
    std::fs::write(&file, "#!/bin/bash\necho hello\n")?;

    let mut bridge = BridgeProcess::spawn(&[], dir.path().to_str().context("path")?)?;
    bridge.initialize()?;

    let response = bridge.call_diagnostics(1, file.to_str().context("path")?)?;
    let text = response
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");

    assert!(
        text.contains("mock diagnostic"),
        "ProcessMonitor path should return diagnostics. Got: {text}"
    );

    Ok(())
}

/// mockls with `--publish-version`: includes version field in
/// publishDiagnostics. Exercises the Version strategy.
#[test]
fn test_diagnostics_version_path() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("test.sh");
    std::fs::write(&file, "#!/bin/bash\necho hello\n")?;

    let mut bridge =
        BridgeProcess::spawn(&["--publish-version"], dir.path().to_str().context("path")?)?;
    bridge.initialize()?;

    let response = bridge.call_diagnostics(1, file.to_str().context("path")?)?;
    let text = response
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");

    assert!(
        text.contains("mock diagnostic"),
        "Version path should return diagnostics. Got: {text}"
    );

    Ok(())
}

/// mockls with `--progress-on-change`: sends progress tokens around
/// diagnostic computation. Exercises the `TokenMonitor` strategy.
#[test]
fn test_diagnostics_token_monitor_path() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("test.sh");
    std::fs::write(&file, "#!/bin/bash\necho hello\n")?;

    let mut bridge = BridgeProcess::spawn(
        &["--progress-on-change"],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    let response = bridge.call_diagnostics(1, file.to_str().context("path")?)?;
    let text = response
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");

    assert!(
        text.contains("mock diagnostic"),
        "TokenMonitor path should return diagnostics. Got: {text}"
    );

    Ok(())
}

/// mockls with `--drop-after 2`: crashes after 2 responses (initialize
/// + shutdown or first tool call). Verifies `ServerDied` is handled.
#[test]
fn test_diagnostics_server_death() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("test.sh");
    std::fs::write(&file, "#!/bin/bash\necho hello\n")?;

    let mut bridge =
        BridgeProcess::spawn(&["--drop-after", "2"], dir.path().to_str().context("path")?)?;
    bridge.initialize()?;

    // Server will die during or before diagnostics processing
    let response = bridge.call_diagnostics(1, file.to_str().context("path")?)?;
    let text = response
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");

    // Should either get diagnostics (if server published before dying),
    // "No diagnostics", or an error about the server dying
    let is_acceptable = text.contains("mock diagnostic")
        || text.contains("No diagnostics")
        || text.contains("server")
        || response.get("error").is_some();

    assert!(
        is_acceptable,
        "Server death should be handled gracefully. Got: {response}"
    );

    Ok(())
}

/// mockls with `--progress-on-change --no-diagnostics`: server sends
/// progress tokens but never publishes diagnostics. The `TokenMonitor`
/// should detect Active → Idle and return cached (empty) diagnostics.
#[test]
fn test_diagnostics_token_monitor_no_diagnostics() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("test.sh");
    std::fs::write(&file, "#!/bin/bash\necho hello\n")?;

    let mut bridge = BridgeProcess::spawn(
        &["--progress-on-change", "--no-diagnostics"],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    let response = bridge.call_diagnostics(1, file.to_str().context("path")?)?;
    let text = response
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");

    assert!(
        text.contains("No diagnostics"),
        "TokenMonitor with no diagnostics should return empty. Got: {text}"
    );

    Ok(())
}

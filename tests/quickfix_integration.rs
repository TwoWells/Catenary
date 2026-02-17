#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Integration tests for auto-fix functionality.

use anyhow::{Context, Result};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Helper to spawn the bridge
struct BridgeProcess {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<BufReader<std::process::ChildStdout>>,
}

impl BridgeProcess {
    fn spawn(root: &str) -> Result<Self> {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        // Enable rust-analyzer
        cmd.arg("--lsp").arg("rust:rust-analyzer");
        cmd.arg("--root").arg(root);
        // Isolate from user-level config
        cmd.env("XDG_CONFIG_HOME", root);

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = cmd.spawn().context("Failed to spawn bridge")?;
        let stdin = child.stdin.take().context("Failed to get stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("Failed to get stdout")?);

        // Wait for initialization
        std::thread::sleep(Duration::from_millis(500));

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
        })
    }

    fn send(&mut self, request: &serde_json::Value) -> Result<()> {
        let json = serde_json::to_string(request)?;
        let stdin = self.stdin.as_mut().context("Stdin already closed")?;
        writeln!(stdin, "{json}").context("Failed to write to stdin")?;
        stdin.flush().context("Failed to flush stdin")?;
        Ok(())
    }

    fn recv(&mut self) -> Result<serde_json::Value> {
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
                "clientInfo": { "name": "test", "version": "1.0" }
            }
        }))?;
        let _ = self.recv()?;
        self.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;
        Ok(())
    }
}

impl Drop for BridgeProcess {
    fn drop(&mut self) {
        // Closing stdin signals the server to shut down gracefully
        self.stdin.take();

        // Wait for the process to exit naturally (up to 2 seconds)
        for _ in 0..20 {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        // If still alive after timeout, kill it
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn test_quickfix_rust_unused() -> Result<()> {
    // Requires rust-analyzer
    if Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_err()
    {
        return Ok(());
    }

    let temp = tempfile::tempdir()?;
    // Create Cargo.toml
    std::fs::write(
        temp.path().join("Cargo.toml"),
        r#"[package]
name = "test-quickfix"
version = "0.1.0"
edition = "2021"
"#,
    )?;

    std::fs::create_dir(temp.path().join("src"))?;
    let main_rs = temp.path().join("src/main.rs");

    // Unused variable 'x'
    let content = "fn main() {\n    let x = 1;\n}\n";
    std::fs::write(&main_rs, content)?;

    let mut bridge = BridgeProcess::spawn(temp.path().to_str().context("invalid path")?)?;
    bridge.initialize()?;

    // Give LSP time to index and lint (Rust Analyzer takes a bit)
    let mut found_diagnostic = false;
    for _ in 0..20 {
        // 10 seconds
        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": 999,
            "method": "tools/call",
            "params": {
                "name": "diagnostics",
                "arguments": {
                    "file": main_rs.to_str().context("invalid path")?
                }
            }
        }))?;
        let response = bridge.recv()?;
        if let Some(content) = response["result"]["content"][0]["text"].as_str() {
            tracing::debug!("Diagnostics: {content}");
            if content.contains("unused") || content.contains('x') {
                found_diagnostic = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    assert!(found_diagnostic, "Expected unused variable warning");

    // Request quickfix at line 1 ("let x = 1;"), char 8 (on "x")
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "apply_quickfix",
            "arguments": {
                "file": main_rs.to_str().context("invalid path")?,
                "line": 1,
                "character": 8
            }
        }
    }))?;

    let response = bridge.recv()?;
    tracing::debug!("Response: {response:?}");
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Tool call failed"
    );

    // Verify the response contains proposed edits (not applied to disk)
    let response_text = result["content"][0]["text"].as_str().unwrap_or_default();
    tracing::debug!("Response text: {response_text}");

    assert!(
        response_text.contains("Proposed fix:") && response_text.contains("_x"),
        "Expected proposed edits with _x prefix, got: {response_text}"
    );

    // Verify the file was NOT modified (edits are proposed, not applied)
    let new_content = std::fs::read_to_string(&main_rs)?;
    assert_eq!(
        new_content, content,
        "File should not have been modified — edits are proposed only"
    );
    Ok(())
}

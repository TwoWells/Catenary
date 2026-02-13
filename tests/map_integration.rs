#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Integration tests for codebase map tool.

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
    fn spawn(root: &str, lsp_args: Option<&str>) -> Result<Self> {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        if let Some(arg) = lsp_args {
            cmd.arg("--lsp").arg(arg);
        } else {
            // Default for existing tests
            cmd.arg("--lsp")
                .arg("shellscript:bash-language-server start");
        }
        cmd.arg("--root").arg(root);

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
fn test_codebase_map_basic() -> Result<()> {
    let temp = tempfile::tempdir()?;
    std::fs::write(temp.path().join("file1.txt"), "content")?;
    std::fs::create_dir(temp.path().join("subdir"))?;
    std::fs::write(temp.path().join("subdir/file2.rs"), "fn main() {}")?;

    let mut bridge = BridgeProcess::spawn(temp.path().to_str().context("invalid path")?, None)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "catenary_codebase_map",
            "arguments": {
                "path": temp.path().to_str().context("invalid path")?,
                "max_depth": 5,
                "include_symbols": false
            }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(result["isError"].is_null() || result["isError"] == false);

    let content = result["content"][0]["text"]
        .as_str()
        .context("Missing text in content")?;
    tracing::debug!("Map Output:\n{content}");

    assert!(content.contains("file1.txt"));
    assert!(content.contains("subdir/"));
    assert!(content.contains("file2.rs"));
    Ok(())
}

#[test]
fn test_codebase_map_with_symbols() -> Result<()> {
    // Requires bash-language-server
    if Command::new("which")
        .arg("bash-language-server")
        .output()
        .is_err()
    {
        return Ok(());
    }

    let temp = tempfile::tempdir()?;
    let script = temp.path().join("script.sh");
    std::fs::write(&script, "#!/bin/bash\nfunction my_func() { echo hi; }\n")?;

    let mut bridge = BridgeProcess::spawn(temp.path().to_str().context("invalid path")?, None)?;
    bridge.initialize()?;

    // Give LSP time to wake up if lazy
    std::thread::sleep(Duration::from_millis(500));

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "catenary_codebase_map",
            "arguments": {
                "path": temp.path().to_str().context("invalid path")?,
                "include_symbols": true
            }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    let content = result["content"][0]["text"]
        .as_str()
        .context("Missing text in content")?;

    tracing::debug!("Map with Symbols Output:\n{content}");

    assert!(content.contains("script.sh"));
    // Bash LSP should report 'my_func' as a Function
    if !content.contains("my_func") {
        tracing::warn!(
            "WARNING: Symbols not found. Check if bash-language-server is running correctly."
        );
    }
    Ok(())
}

#[test]
fn test_codebase_map_markdown() -> Result<()> {
    if Command::new("which").arg("marksman").output().is_err() {
        return Ok(());
    }

    let temp = tempfile::tempdir()?;
    // Create .git to help Marksman detect root
    std::fs::create_dir(temp.path().join(".git"))?;

    let md_path = temp.path().join("README.md");
    std::fs::write(
        &md_path,
        "# Title\n\n## Section 1\nContent\n\n### Subsection\nMore content",
    )?;

    let mut bridge = BridgeProcess::spawn(
        temp.path().to_str().context("invalid path")?,
        Some("markdown:marksman server"),
    )?;
    bridge.initialize()?;

    // Give LSP time to scan
    std::thread::sleep(Duration::from_secs(5));

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "catenary_codebase_map",
            "arguments": {
                "path": temp.path().to_str().context("invalid path")?,
                "include_symbols": true
            }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    let content = result["content"][0]["text"]
        .as_str()
        .context("Missing text in content")?;

    tracing::debug!("Markdown Map Output:\n{content}");

    assert!(content.contains("README.md"));
    // Marksman usually reports headings as symbols
    assert!(content.contains("Title"), "Should contain Title symbol");
    assert!(
        content.contains("Section 1"),
        "Should contain Section 1 symbol"
    );
    Ok(())
}

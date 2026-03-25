// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for the `glob` tool (directory/file/pattern modes).

use anyhow::{Context, Result};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

const MOCK_LANG_A: &str = "yX4Za";

/// Helper to spawn the bridge
struct BridgeProcess {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<BufReader<std::process::ChildStdout>>,
}

impl BridgeProcess {
    fn spawn(root: &str, lsp_args: Option<&str>) -> Result<Self> {
        Self::spawn_multi_root(&[root], lsp_args)
    }

    fn spawn_multi_root(roots: &[&str], lsp_args: Option<&str>) -> Result<Self> {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        if let Some(arg) = lsp_args {
            cmd.arg("--lsp").arg(arg);
        } else {
            let bin = env!("CARGO_BIN_EXE_mockls");
            cmd.arg("--lsp")
                .arg(format!("{MOCK_LANG_A}:{bin} {MOCK_LANG_A}"));
        }

        // Set roots via env var
        let roots_val = std::env::join_paths(roots).unwrap_or_default();
        cmd.env("CATENARY_ROOTS", &roots_val);

        // Isolate from user-level config and state
        if let Some(first_root) = roots.first() {
            cmd.env("XDG_CONFIG_HOME", first_root);
            cmd.env("XDG_STATE_HOME", first_root);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = cmd.spawn().context("Failed to spawn bridge")?;
        let stdin = child.stdin.take().context("Failed to get stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("Failed to get stdout")?);

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
fn test_glob_directory_basic() -> Result<()> {
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
            "name": "glob",
            "arguments": {
                "pattern": temp.path().to_str().context("invalid path")?
            }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(result["isError"].is_null() || result["isError"] == false);

    let content = result["content"][0]["text"]
        .as_str()
        .context("Missing text in content")?;

    assert!(
        content.contains("file1.txt"),
        "Should list file1.txt, got:\n{content}"
    );
    assert!(
        content.contains("subdir/"),
        "Should list subdir/, got:\n{content}"
    );
    Ok(())
}

#[test]
fn test_glob_directory_symbols() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let script = temp.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(&script, "struct Config\nenum Mode\nconst MAX_SIZE\n")?;

    let mut bridge = BridgeProcess::spawn(temp.path().to_str().context("invalid path")?, None)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "glob",
            "arguments": {
                "pattern": temp.path().to_str().context("invalid path")?
            }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    let content = result["content"][0]["text"]
        .as_str()
        .context("Missing text in content")?;

    assert!(
        content.contains(&format!("types.{MOCK_LANG_A}")),
        "Should list the file, got:\n{content}"
    );
    assert!(
        content.contains("Config"),
        "Should contain Config symbol, got:\n{content}"
    );
    assert!(
        content.contains("Mode"),
        "Should contain Mode symbol, got:\n{content}"
    );
    Ok(())
}

/// Verifies that glob returns outline symbols for a single file,
/// filtering to outline kinds only.
#[test]
fn test_glob_file_outline() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let script = temp.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(
        &script,
        "struct Config\nenum Mode\nconst MAX_SIZE\nfn do_work\n",
    )?;

    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let lsp = format!("{MOCK_LANG_A}:{mockls_bin} {MOCK_LANG_A}");

    let mut bridge =
        BridgeProcess::spawn(temp.path().to_str().context("invalid path")?, Some(&lsp))?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "glob",
            "arguments": {
                "pattern": script.to_str().context("file path")?
            }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    let content = result["content"][0]["text"]
        .as_str()
        .context("Missing text in content")?;

    // Outline kinds should be present
    assert!(
        content.contains("Config"),
        "Should contain Config symbol, got:\n{content}"
    );
    assert!(
        content.contains("Struct"),
        "Config should have Struct kind, got:\n{content}"
    );
    assert!(
        content.contains("Mode"),
        "Should contain Mode symbol, got:\n{content}"
    );
    assert!(
        content.contains("Enum"),
        "Mode should have Enum kind, got:\n{content}"
    );
    assert!(
        content.contains("MAX_SIZE"),
        "Should contain MAX_SIZE symbol, got:\n{content}"
    );
    assert!(
        content.contains("Constant"),
        "MAX_SIZE should have Constant kind, got:\n{content}"
    );

    // Function kind should be excluded from outline
    assert!(
        !content.contains("do_work"),
        "Function 'do_work' should be excluded from outline, got:\n{content}"
    );

    // Line numbers should be present
    assert!(
        content.contains("L1"),
        "Should contain L1 line number, got:\n{content}"
    );
    assert!(
        content.contains("L2"),
        "Should contain L2 line number, got:\n{content}"
    );

    // Line count header
    assert!(
        content.contains("(4 lines)"),
        "Should show line count, got:\n{content}"
    );
    Ok(())
}

#[test]
fn test_glob_directory_explicit_path() -> Result<()> {
    // When an explicit path is given, even in multi-root mode, only that path is shown
    let dir_a = tempfile::tempdir()?;
    let dir_b = tempfile::tempdir()?;

    std::fs::write(dir_a.path().join("only_a.txt"), "a")?;
    std::fs::write(dir_b.path().join("only_b.txt"), "b")?;

    let root_a = dir_a.path().to_str().context("invalid path A")?;
    let root_b = dir_b.path().to_str().context("invalid path B")?;

    let mut bridge = BridgeProcess::spawn_multi_root(&[root_a, root_b], None)?;
    bridge.initialize()?;

    // Request glob with explicit path pointing to root A only
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "tools/call",
        "params": {
            "name": "glob",
            "arguments": {
                "pattern": root_a
            }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "glob with explicit path failed: {response:?}"
    );

    let content = result["content"][0]["text"]
        .as_str()
        .context("Missing text in content")?;

    assert!(
        content.contains("only_a.txt"),
        "Should contain only_a.txt from explicit path, got:\n{content}"
    );
    assert!(
        !content.contains("only_b.txt"),
        "Should NOT contain only_b.txt when explicit path is root A, got:\n{content}"
    );

    Ok(())
}

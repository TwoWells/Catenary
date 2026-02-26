// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Regression test for issue #3: LSP diagnostics timing.
//!
//! Verifies that Catenary correctly waits for the LSP server to complete
//! its analysis after a file change before returning diagnostics,
//! ensuring accuracy and avoiding race conditions.
//!
//! Uses mockls + mockc to deterministically simulate the flycheck pattern
//! (LSP sleeping while subprocess burns CPU), replacing the original
//! rust-analyzer test that was flaky under load.

use anyhow::{Context, Result};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

/// Helper matching the `BridgeProcess` pattern in other test files.
struct BridgeProcess {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<BufReader<std::process::ChildStdout>>,
}

impl BridgeProcess {
    fn spawn(lsp: &str, root: &str) -> Result<Self> {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        cmd.arg("--lsp")
            .arg(lsp)
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
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))?;
        std::thread::sleep(std::time::Duration::from_millis(100));
        Ok(())
    }
}

impl Drop for BridgeProcess {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.wait();
    }
}

/// Simulates the flycheck pattern: file change → diagnostics request.
///
/// mockls with `--publish-version --advertise-save --flycheck-command` wraps
/// the subprocess in a `$/progress` bracket. Catenary's `TokenMonitor` waits
/// for the progress cycle to complete before returning diagnostics.
///
/// mockc `--ticks 5` burns 5 centiseconds (~50ms) of CPU — enough to
/// exercise the scheduling pattern without slowing tests.
#[test]
fn test_lsp_diagnostics_waits_for_analysis_after_change() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_path = dir.path().join("test.sh");
    std::fs::write(&file_path, "#!/bin/bash\necho hello\n")?;

    let mockc_bin = env!("CARGO_BIN_EXE_mockc");
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    // mockc defaults to --ticks 10 (~100ms CPU). No extra args needed,
    // avoiding quoting issues with Catenary's whitespace-split --lsp parser.
    let lsp = format!(
        "shellscript:{mockls_bin} --publish-version --advertise-save \
         --flycheck-command {mockc_bin}"
    );

    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&lsp, root)?;
    bridge.initialize()?;

    // First diagnostics call — opens the file, triggers didOpen diagnostics
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "diagnostics",
            "arguments": {
                "file": file_path.to_str().context("file path")?
            }
        }
    }))?;
    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        text.contains("mock diagnostic"),
        "Initial diagnostics should contain mock diagnostic, got: {text}"
    );

    // Change the file on disk — simulates the agent editing the file
    std::fs::write(&file_path, "#!/bin/bash\necho changed\necho line3\n")?;

    // Second diagnostics call IMMEDIATELY after change.
    // This triggers didChange + didSave. The flycheck subprocess (mockc)
    // runs under a progress bracket. Catenary should wait for the full
    // Active→Idle cycle before returning diagnostics.
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "diagnostics",
            "arguments": {
                "file": file_path.to_str().context("file path")?
            }
        }
    }))?;

    let response = bridge.recv()?;
    assert!(
        response["result"]["isError"] != true,
        "Diagnostics should not error: {response:?}"
    );
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        text.contains("mock diagnostic"),
        "Post-change diagnostics should contain mock diagnostic (after flycheck), got: {text}"
    );

    Ok(())
}

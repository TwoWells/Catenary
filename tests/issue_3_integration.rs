// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Regression test for issue #3: LSP diagnostics timing.
//!
//! Verifies that Catenary correctly waits for the LSP server to complete
//! its analysis after a file change before returning diagnostics,
//! ensuring accuracy and avoiding race conditions.
//!
//! Uses mockls + mockc to deterministically simulate the flycheck pattern
//! (LSP sleeping while subprocess burns CPU), replacing the original
//! rust-analyzer test that was flaky under load.

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

const MOCK_LANG_A: &str = "yX4Za";

/// Helper matching the `BridgeProcess` pattern in other test files.
struct BridgeProcess {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<BufReader<std::process::ChildStdout>>,
    state_home: String,
}

impl BridgeProcess {
    fn spawn(lsp: &str, root: &str) -> Result<Self> {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        cmd.arg("--lsp")
            .arg(lsp)
            .arg("--root")
            .arg(root)
            .env("XDG_CONFIG_HOME", root)
            .env("XDG_STATE_HOME", root)
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
            state_home: root.to_string(),
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
        std::thread::sleep(Duration::from_millis(100));
        Ok(())
    }

    /// Sends a file-change notification via the notify socket and returns
    /// the diagnostics text.
    fn call_diagnostics_via_notify(&self, file: &str) -> Result<String> {
        let sessions_dir = PathBuf::from(&self.state_home)
            .join("catenary")
            .join("sessions");
        let socket_path = find_notify_socket(&sessions_dir)?;

        let mut stream = std::os::unix::net::UnixStream::connect(&socket_path)
            .context("connect to notify socket")?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .context("set read timeout")?;

        let request = json!({"file": file});
        writeln!(stream, "{request}").context("write to notify socket")?;
        stream
            .shutdown(std::net::Shutdown::Write)
            .context("shutdown write")?;

        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .context("read from notify socket")?;

        // Unwrap NotifyResult wire protocol — return the content string
        let trimmed = response.trim();
        serde_json::from_str::<catenary_mcp::hook::NotifyResult>(trimmed).map_or_else(
            |_| Ok(trimmed.to_string()),
            |result| match result {
                catenary_mcp::hook::NotifyResult::Content(s) => Ok(s),
                catenary_mcp::hook::NotifyResult::Error(e) => Ok(format!("Notify error: {e}")),
            },
        )
    }
}

impl Drop for BridgeProcess {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.wait();
    }
}

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
    let file_path = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file_path, "echo hello\n")?;

    let mockc_bin = env!("CARGO_BIN_EXE_mockc");
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    // mockc defaults to --ticks 10 (~100ms CPU). No extra args needed,
    // avoiding quoting issues with Catenary's whitespace-split --lsp parser.
    let lsp = format!(
        "{MOCK_LANG_A}:{mockls_bin} {MOCK_LANG_A} --publish-version --advertise-save \
         --flycheck-command {mockc_bin}"
    );

    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&lsp, root)?;
    bridge.initialize()?;

    // First diagnostics call — opens the file, triggers didOpen diagnostics
    let text = bridge.call_diagnostics_via_notify(file_path.to_str().context("file path")?)?;
    assert!(
        text.contains("mock diagnostic"),
        "Initial diagnostics should contain mock diagnostic, got: {text}"
    );

    // Change the file on disk — simulates the agent editing the file
    std::fs::write(&file_path, "echo changed\necho line3\n")?;

    // Second diagnostics call IMMEDIATELY after change.
    // This triggers didChange + didSave. The flycheck subprocess (mockc)
    // runs under a progress bracket. Catenary should wait for the full
    // Active→Idle cycle before returning diagnostics.
    let text = bridge.call_diagnostics_via_notify(file_path.to_str().context("file path")?)?;
    assert!(
        text.contains("mock diagnostic"),
        "Post-change diagnostics should contain mock diagnostic (after flycheck), got: {text}"
    );

    Ok(())
}

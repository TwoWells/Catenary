// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for the diagnostics pipeline.
//!
//! Uses mockls with various flags to exercise pipeline behavior:
//! - Default (settle + push cache)
//! - Version matching (`--publish-version`)
//! - Progress tokens (`--progress-on-change`)
//! - Pull-only (`--pull-diagnostics --no-push-diagnostics`)
//! - Server death (`--drop-after`)

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

const MOCK_LANG_A: &str = "yX4Za";

/// Helper to spawn the bridge with mockls and communicate via MCP.
struct BridgeProcess {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<BufReader<std::process::ChildStdout>>,
    state_home: Option<String>,
}

impl BridgeProcess {
    fn spawn(mockls_args: &[&str], root: &str) -> Result<Self> {
        Self::spawn_with_state_home(mockls_args, root, root)
    }

    fn spawn_with_state_home(mockls_args: &[&str], root: &str, state_home: &str) -> Result<Self> {
        let mockls_bin = env!("CARGO_BIN_EXE_mockls");
        let mut lsp_cmd = format!("{MOCK_LANG_A}:{mockls_bin} {MOCK_LANG_A}");
        for arg in mockls_args {
            lsp_cmd.push(' ');
            lsp_cmd.push_str(arg);
        }

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        cmd.env("CATENARY_SERVERS", &lsp_cmd)
            .env("CATENARY_ROOTS", root)
            .env("XDG_CONFIG_HOME", root)
            .env("XDG_STATE_HOME", state_home)
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
            state_home: Some(state_home.to_string()),
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

    /// Sends a file-change notification via the hook socket and returns
    /// the diagnostics text. This exercises the production hook path
    /// (`catenary hook post-tool`) rather than the (removed) MCP `diagnostics` tool.
    fn call_diagnostics_via_notify(&self, file: &str) -> Result<String> {
        use std::io::Read as _;

        let state_home = self.state_home.as_ref().context("state_home not set")?;
        let sessions_dir = PathBuf::from(state_home).join("catenary").join("sessions");
        let socket_path = find_notify_socket(&sessions_dir)?;

        let mut stream = std::os::unix::net::UnixStream::connect(&socket_path)
            .context("connect to notify socket")?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .context("set read timeout")?;

        let request = json!({"method": "post-tool/diagnostics", "file": file});
        writeln!(stream, "{request}").context("write to notify socket")?;
        stream
            .shutdown(std::net::Shutdown::Write)
            .context("shutdown write")?;

        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .context("read from notify socket")?;

        // Unwrap HookResult wire protocol — return the content string
        let trimmed = response.trim();
        serde_json::from_str::<catenary_mcp::hook::HookResult>(trimmed).map_or_else(
            |_| Ok(trimmed.to_string()),
            |result| match result {
                catenary_mcp::hook::HookResult::Content(s)
                | catenary_mcp::hook::HookResult::Courtesy(s) => Ok(s),
                catenary_mcp::hook::HookResult::Error(e) => Ok(format!("Notify error: {e}")),
                other => Ok(format!("{other:?}")),
            },
        )
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
/// version or progress tokens. With settle-based pipeline, diagnostics
/// are retrieved after the server process tree goes quiet — no strategy
/// discovery needed.
#[test]
fn test_diagnostics_default_mockls() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = BridgeProcess::spawn(&[], dir.path().to_str().context("path")?)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics_via_notify(file.to_str().context("path")?)?;

    assert!(
        text.contains("mock diagnostic"),
        "Default mockls should return diagnostics via settle + push cache. Got: {text}"
    );

    Ok(())
}

/// mockls with `--publish-version`: includes version field in
/// publishDiagnostics. Exercises the Version strategy.
#[test]
fn test_diagnostics_version_path() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge =
        BridgeProcess::spawn(&["--publish-version"], dir.path().to_str().context("path")?)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics_via_notify(file.to_str().context("path")?)?;

    assert!(
        text.contains("mock diagnostic"),
        "Version path should return diagnostics. Got: {text}"
    );

    Ok(())
}

/// mockls with `--progress-on-change`: sends progress tokens around
/// diagnostic computation on `didChange`. Exercises the `TokenMonitor` strategy.
///
/// Progress tokens are only sent on `didChange` (not `didOpen`), so
/// the first call opens the file (degraded mode), and the second call
/// after modification triggers the progress path.
#[test]
fn test_diagnostics_token_monitor_path() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = BridgeProcess::spawn(
        &["--progress-on-change"],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    // First call: opens the file via didOpen (no progress tokens sent)
    let _ = bridge.call_diagnostics_via_notify(file.to_str().context("path")?)?;

    // Modify file to trigger didChange on next call
    std::fs::write(&file, "echo changed\necho line3\n")?;

    // Second call: triggers didChange → progress tokens → TokenMonitor
    let text = bridge.call_diagnostics_via_notify(file.to_str().context("path")?)?;

    assert!(
        text.contains("mock diagnostic"),
        "TokenMonitor path should return diagnostics on didChange. Got: {text}"
    );

    Ok(())
}

/// mockls with `--drop-after 2`: crashes after 2 responses (initialize
/// + shutdown or first tool call). Verifies `ServerDied` is handled.
#[test]
fn test_diagnostics_server_death() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge =
        BridgeProcess::spawn(&["--drop-after", "2"], dir.path().to_str().context("path")?)?;
    bridge.initialize()?;

    // Server will die during or before diagnostics processing
    let text = bridge
        .call_diagnostics_via_notify(file.to_str().context("path")?)
        .unwrap_or_default();

    // Should either get diagnostics (if server published before dying),
    // a status message, or a notify error. No raw infrastructure messages to agent.
    let is_acceptable = text.contains("mock diagnostic")
        || text == "[no language server]"
        || text == "[clean]"
        || text.contains("Notify error");

    assert!(
        is_acceptable,
        "Server death should be handled gracefully. Got: {text}"
    );

    Ok(())
}

/// mockls with `--publish-version --no-code-actions`: server does not
/// advertise `codeActionProvider`. Diagnostics should appear without
/// any `fix:` lines (the capability gate in `process_file_inner` skips
/// code action requests entirely).
#[test]
fn test_diagnostics_no_code_actions() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = BridgeProcess::spawn(
        &["--publish-version", "--no-code-actions"],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics_via_notify(file.to_str().context("path")?)?;

    assert!(
        text.contains("mock diagnostic"),
        "Should contain diagnostics. Got: {text}"
    );
    assert!(
        !text.contains("fix:"),
        "Should NOT contain fix: lines when code actions are disabled. Got: {text}"
    );

    Ok(())
}

/// mockls with `--publish-version --multi-fix`: server returns multiple
/// quickfix actions per diagnostic. Each diagnostic should have two
/// `fix:` lines (the primary and the alternative).
#[test]
fn test_diagnostics_multi_fix() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = BridgeProcess::spawn(
        &["--publish-version", "--multi-fix"],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics_via_notify(file.to_str().context("path")?)?;

    assert!(
        text.contains("mock diagnostic"),
        "Should contain diagnostics. Got: {text}"
    );

    let fix_count = text.lines().filter(|l| l.contains("fix:")).count();
    assert!(
        fix_count >= 2,
        "Multi-fix mode should produce at least 2 fix: lines. Got {fix_count} in: {text}"
    );
    assert!(
        text.contains("fix: alternative for"),
        "Should contain alternative fix. Got: {text}"
    );

    Ok(())
}

/// Default mockls with `--publish-version` now always includes a
/// `refactor` code action alongside quickfix actions. Verify that
/// refactor actions are filtered out and only `fix:` lines from
/// quickfix actions appear in the output.
#[test]
fn test_diagnostics_refactor_filtered() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge =
        BridgeProcess::spawn(&["--publish-version"], dir.path().to_str().context("path")?)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics_via_notify(file.to_str().context("path")?)?;

    assert!(
        text.contains("fix:"),
        "Should contain quickfix fix: lines. Got: {text}"
    );
    assert!(
        !text.contains("refactor"),
        "Refactor actions should be filtered out. Got: {text}"
    );

    Ok(())
}

/// mockls with `--pull-diagnostics --no-push-diagnostics`: server advertises
/// pull diagnostics but never pushes. Verifies that Catenary uses the pull
/// path to retrieve diagnostics instead of returning `[diagnostics unavailable]`.
#[test]
fn test_diagnostics_pull_only() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = BridgeProcess::spawn(
        &["--pull-diagnostics", "--no-push-diagnostics"],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics_via_notify(file.to_str().context("path")?)?;

    assert!(
        text.contains("mock diagnostic"),
        "Pull-only server should return diagnostics via pull path. Got: {text}"
    );

    Ok(())
}

/// Scans the sessions directory for a `notify.sock` file.
fn find_notify_socket(sessions_dir: &std::path::Path) -> Result<PathBuf> {
    // Poll briefly for the socket to appear (bridge may still be starting)
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

/// Verifies that quick-fix code actions from the LSP server appear as
/// `fix:` lines in the hook diagnostics output.
///
/// mockls advertises `codeActionProvider: true` and returns quickfix
/// code actions for diagnostics with source "mockls".
#[test]
fn test_diagnostics_code_action_enrichment() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge =
        BridgeProcess::spawn(&["--publish-version"], dir.path().to_str().context("path")?)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics_via_notify(file.to_str().context("path")?)?;

    // mockls publishes diagnostics with source "mockls" and returns
    // quickfix code actions with title "fix: <message>" for those.
    assert!(
        text.contains("mock diagnostic"),
        "Should contain diagnostics. Got: {text}"
    );
    assert!(
        text.contains("fix:"),
        "Should contain fix: lines from code actions. Got: {text}"
    );

    Ok(())
}

/// mockls with `--publish-version --advertise-save --flycheck-command mockc`:
/// Exercises the multi-round diagnostics pattern (Gap 1). After `didSave`,
/// mockls spawns mockc as a subprocess under a `$/progress` bracket. Native
/// diagnostics arrive immediately; flycheck diagnostics arrive after mockc
/// finishes. Catenary should wait for the full Active→Idle progress cycle,
/// returning flycheck diagnostics (which contain "flycheck") rather than
/// short-circuiting on the first native diagnostics.
#[test]
fn test_diagnostics_flycheck_multi_round() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mockc_bin = env!("CARGO_BIN_EXE_mockc");
    let mut bridge = BridgeProcess::spawn(
        &[
            "--publish-version",
            "--advertise-save",
            "--flycheck-command",
            mockc_bin,
        ],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    // First call: opens the file (native diagnostics only, no flycheck)
    let _ = bridge.call_diagnostics_via_notify(file.to_str().context("path")?)?;

    // Modify file to trigger didChange + didSave on next call
    std::fs::write(&file, "echo changed\necho line3\n")?;

    // Second call: triggers didChange + didSave → flycheck subprocess
    let text = bridge.call_diagnostics_via_notify(file.to_str().context("path")?)?;

    // Should contain diagnostics reflecting the modified file (2 lines).
    // The flycheck subprocess runs under a progress bracket; Catenary must
    // wait for the full Active→Idle cycle to get the post-flycheck diagnostics.
    assert!(
        text.contains("mock diagnostic") && text.contains("2 lines"),
        "Multi-round path should return flycheck diagnostics for \
         the modified file (2 lines). Got: {text}"
    );

    Ok(())
}

/// mockls with `--progress-on-change --no-push-diagnostics`: server sends
/// progress tokens but never publishes diagnostics. After settle, the push
/// cache is empty and pull is not supported → `[clean]`.
#[test]
fn test_diagnostics_no_push_no_pull_returns_clean() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mut bridge = BridgeProcess::spawn(
        &["--progress-on-change", "--no-push-diagnostics"],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics_via_notify(file.to_str().context("path")?)?;

    assert_eq!(
        text, "[clean]",
        "Server with no push and no pull should return [clean] after settle. Got: {text}"
    );

    Ok(())
}

/// Near-threshold flycheck: mockc burns 900 ticks (~9s wall time) under
/// a `$/progress` bracket. mockls is Sleeping while the subprocess runs,
/// so the threshold does not drain (subprocess ticks don't count against
/// mockls). After mockc finishes, mockls publishes diagnostics with a
/// version match.
#[test]
fn test_near_threshold_flycheck() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "echo hello\n")?;

    let mockc_bin = env!("CARGO_BIN_EXE_mockc");
    let mut bridge = BridgeProcess::spawn(
        &[
            "--publish-version",
            "--advertise-save",
            "--flycheck-command",
            mockc_bin,
            "--flycheck-ticks",
            "900",
        ],
        dir.path().to_str().context("path")?,
    )?;
    bridge.initialize()?;

    // First call opens the file and gets initial diagnostics
    let text = bridge.call_diagnostics_via_notify(file.to_str().context("path")?)?;
    assert!(
        text.contains("mock diagnostic"),
        "Initial diagnostics should arrive. Got: {text}"
    );

    // Modify the file to trigger flycheck on the second call
    std::fs::write(&file, "echo changed\necho line3\n")?;

    // Second call: triggers didChange + didSave → flycheck with 900-tick mockc
    let text = bridge.call_diagnostics_via_notify(file.to_str().context("path")?)?;

    assert!(
        text.contains("mock diagnostic"),
        "Near-threshold flycheck should return diagnostics (mockls sleeps \
         while mockc runs, threshold not drained). Got: {text}"
    );

    Ok(())
}

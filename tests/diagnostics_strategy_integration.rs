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
use std::path::PathBuf;
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
        Self::spawn_with_state_home(mockls_args, root, root)
    }

    fn spawn_with_state_home(mockls_args: &[&str], root: &str, state_home: &str) -> Result<Self> {
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

/// Reproduces a cross-change stale diagnostics leak at the `LspClient` level.
///
/// Directly exercises `wait_for_diagnostics_update` with controlled timing.
/// The race: v1's delayed diagnostics arrive during v2's wait, satisfy the
/// generation check (Phase 1), and Phase 2 settles before v2's own diagnostics
/// arrive. The cache holds stale v1 data returned for v2's content.
///
/// Timeline (5s diagnostics delay):
/// - t=0: didOpen(v1) + didSave → diagnostics queued (arrive at t=5s)
/// - t=4s: didChange(v2) + didSave → snapshot gen=0, diagnostics queued (arrive at t=9s)
/// - t=5s: v1 diagnostics arrive → gen=1 > 0 → Phase 1 exits
/// - t=7s: Phase 2 settle (2s silence) → returns. Cache has v1 data.
/// - t=9s: v2 diagnostics arrive — too late.
#[tokio::test]
async fn test_diagnostics_stale_lsp_client_level() -> Result<()> {
    use catenary_mcp::lsp::{DiagnosticsWaitResult, LspClient};
    use catenary_mcp::session::EventBroadcaster;
    use lsp_types::Uri;
    use std::str::FromStr;

    let dir = tempfile::tempdir()?;
    let file = dir.path().join("test.sh");

    // Content v1: 2 lines
    let v1 = "#!/bin/bash\necho v1\n";
    std::fs::write(&file, v1)?;

    let uri_string = format!("file://{}", file.display());
    let uri = Uri::from_str(&uri_string).context("parse URI")?;

    // Spawn LspClient directly with mockls --diagnostics-delay 5000
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let broadcaster = EventBroadcaster::noop()?;
    let mut client = LspClient::spawn(
        mockls_bin,
        &["--diagnostics-delay", "5000", "--publish-version"],
        "shellscript",
        broadcaster,
    )
    .context("spawn LspClient")?;

    // Initialize with the temp dir as workspace root
    client.initialize(&[dir.path().to_path_buf()], None).await?;

    // Wait for server to be ready
    client.wait_ready().await;

    // didOpen(v1) + didSave at t=0
    client
        .did_open(lsp_types::DidOpenTextDocumentParams {
            text_document: lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "shellscript".to_string(),
                version: 1,
                text: v1.to_string(),
            },
        })
        .await?;
    client.did_save(uri.clone()).await?;

    // Sleep 4s — v1 diagnostics haven't arrived yet (5s delay > 4s)
    tokio::time::sleep(Duration::from_secs(4)).await;

    // Snapshot generation before v2 change
    let snapshot = client.diagnostics_generation(&uri).await;
    assert_eq!(snapshot, 0, "No diagnostics should have arrived yet");

    // Content v2: 5 lines
    let v2 = "#!/bin/bash\necho v2\necho line3\necho line4\necho line5\n";
    std::fs::write(&file, v2)?;

    // didChange(v2) + didSave at t≈4s
    client
        .did_change(lsp_types::DidChangeTextDocumentParams {
            text_document: lsp_types::VersionedTextDocumentIdentifier {
                uri: uri.clone(),
                version: 2,
            },
            content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: v2.to_string(),
            }],
        })
        .await?;
    client.did_save(uri.clone()).await?;

    // Wait for diagnostics with snapshot=0 and generous timeout
    let result = client
        .wait_for_diagnostics_update(&uri, snapshot, Duration::from_secs(30))
        .await;

    assert_eq!(result, DiagnosticsWaitResult::Updated);

    // Check what diagnostics we got
    let diagnostics = client.get_diagnostics(&uri).await;
    assert!(
        !diagnostics.is_empty(),
        "Should have some diagnostics. Got none."
    );

    let msg = &diagnostics[0].message;

    // BUG DEMONSTRATION: the diagnostics should reflect v2 (5 lines) but
    // due to the stale leak, they reflect v1 (2 lines).
    assert!(
        msg.contains("(5 lines)"),
        "Diagnostics should reflect current content (5 lines), \
         not stale content from previous version. Got: {msg}"
    );

    Ok(())
}

/// Reproduces a cross-change stale diagnostics leak via concurrent notify socket.
///
/// Exercises the production hook path (`catenary release`) with overlapping
/// connections to the notify socket. Task A opens v1, Task B edits to v2.
/// v1's delayed diagnostics satisfy Task B's generation check, causing stale
/// data to be returned for v2's content.
///
/// Timeline (5s diagnostics delay):
/// - t=0: Task A sends notify for v1 (2 lines) → waits for diagnostics
/// - t=4s: File updated to v2 (5 lines). Task B sends notify → waits
/// - t=5s: v1 diagnostics arrive → Task B's gen check satisfied
/// - t=7s: Task B returns with stale v1 diagnostics
/// - t=9s: v2 diagnostics arrive — too late for Task B
#[tokio::test]
async fn test_diagnostics_stale_notify_socket() -> Result<()> {
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::UnixStream;

    let dir = tempfile::tempdir()?;
    let state_dir = tempfile::tempdir()?;
    let file = dir.path().join("test.sh");

    // Content v1: 2 lines
    std::fs::write(&file, "#!/bin/bash\necho v1\n")?;

    // Spawn bridge with XDG_STATE_HOME so we can find the notify socket
    let root_str = dir.path().to_str().context("path")?;
    let state_str = state_dir.path().to_str().context("state path")?;
    let mut bridge = BridgeProcess::spawn_with_state_home(
        &["--diagnostics-delay", "5000", "--publish-version"],
        root_str,
        state_str,
    )?;
    bridge.initialize()?;

    // Discover the notify socket path
    let sessions_dir = state_dir.path().join("catenary").join("sessions");
    let socket_path = find_notify_socket(&sessions_dir)?;

    let file_path = file.to_str().context("file path")?.to_string();

    // Task A: notify for v1 content (2 lines)
    let socket_a = socket_path.clone();
    let file_a = file_path.clone();
    let task_a = tokio::spawn(async move {
        let stream = UnixStream::connect(&socket_a).await?;
        let (reader, mut writer) = tokio::io::split(stream);
        let request = serde_json::json!({"file": file_a});
        writer.write_all(format!("{request}\n").as_bytes()).await?;
        writer.shutdown().await?;

        let mut response = String::new();
        tokio::io::AsyncReadExt::read_to_string(
            &mut tokio::io::BufReader::new(reader),
            &mut response,
        )
        .await?;
        Ok::<String, anyhow::Error>(response)
    });

    // Sleep 4s — Task A still waiting (diagnostics delayed 5s)
    tokio::time::sleep(Duration::from_secs(4)).await;

    // Modify file to v2: 5 lines
    std::fs::write(
        &file,
        "#!/bin/bash\necho v2\necho line3\necho line4\necho line5\n",
    )?;

    // Task B: notify for v2 content (5 lines)
    let socket_b = socket_path.clone();
    let file_b = file_path.clone();
    let task_b = tokio::spawn(async move {
        let stream = UnixStream::connect(&socket_b).await?;
        let (reader, mut writer) = tokio::io::split(stream);
        let request = serde_json::json!({"file": file_b});
        writer.write_all(format!("{request}\n").as_bytes()).await?;
        writer.shutdown().await?;

        let mut response = String::new();
        tokio::io::AsyncReadExt::read_to_string(
            &mut tokio::io::BufReader::new(reader),
            &mut response,
        )
        .await?;
        Ok::<String, anyhow::Error>(response)
    });

    // Collect results from both tasks
    let response_a = task_a.await.context("Task A panicked")??;
    let response_b = task_b.await.context("Task B panicked")??;

    // Task A should have v1 diagnostics (2 lines) — this is correct
    assert!(
        response_a.contains("mock diagnostic"),
        "Task A should return diagnostics. Got: {response_a}"
    );

    // Task B should have v2 diagnostics (5 lines), not stale v1 (2 lines).
    // BUG DEMONSTRATION: due to the stale leak, Task B gets v1's diagnostics.
    assert!(
        response_b.contains("(5 lines)"),
        "Task B should return diagnostics for current content (5 lines), \
         not stale diagnostics from previous version. Got: {response_b}"
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

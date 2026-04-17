// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! End-to-end integration tests for the MCP-LSP bridge.
//!
//! These tests spawn the actual bridge binary and communicate with it
//! via stdin/stdout using the MCP protocol.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

const MOCK_LANG_A: &str = "yX4Za";
const MOCK_LANG_B: &str = "d5apI";

/// Helper to spawn the bridge and communicate with it
struct BridgeProcess {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<BufReader<std::process::ChildStdout>>,
    stderr: Option<std::process::ChildStderr>,
    state_home: Option<String>,
}

impl BridgeProcess {
    fn spawn(lsp_commands: &[&str], root: &str) -> Result<Self> {
        Self::spawn_multi_root(lsp_commands, &[root])
    }

    fn spawn_multi_root(lsp_commands: &[&str], roots: &[&str]) -> Result<Self> {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));

        // Set servers via env var (semicolon-separated)
        cmd.env("CATENARY_SERVERS", lsp_commands.join(";"));

        // Set roots via env var
        let roots_val = std::env::join_paths(roots).unwrap_or_default();
        cmd.env("CATENARY_ROOTS", &roots_val);

        // Isolate from user-level config
        if let Some(first_root) = roots.first() {
            cmd.env("XDG_CONFIG_HOME", first_root);
            cmd.env("XDG_STATE_HOME", first_root);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().context("Failed to spawn bridge")?;

        let stdin = child.stdin.take().context("Failed to get stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("Failed to get stdout")?);
        let stderr = child.stderr.take();

        let state_home = roots.first().map(std::string::ToString::to_string);

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr,
            state_home,
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
        let n = stdout
            .read_line(&mut line)
            .context("Failed to read from stdout")?;
        if n == 0 {
            // EOF — bridge process died. Capture stderr and exit status.
            let mut stderr_buf = String::new();
            if let Some(ref mut stderr) = self.stderr {
                let _ = stderr.read_to_string(&mut stderr_buf);
            }
            let status = self.child.try_wait().ok().flatten();
            bail!(
                "bridge process closed stdout (EOF). exit status: {status:?}, stderr:\n{stderr_buf}"
            );
        }
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
                    "name": "integration-test",
                    "version": "1.0.0"
                }
            }
        }))?;

        let response = self.recv()?;
        if response.get("result").is_none() {
            bail!("Initialize failed: {response:?}");
        }

        // Send initialized notification
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))?;

        // Small delay for notification processing
        std::thread::sleep(Duration::from_millis(100));
        Ok(())
    }

    /// Initializes with `roots.listChanged` capability.
    ///
    /// After sending `notifications/initialized`, reads the server's
    /// `roots/list` request from stdout and responds with the given roots.
    fn initialize_with_roots(&mut self, roots: &[&str]) -> Result<()> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "roots": { "listChanged": true }
                },
                "clientInfo": {
                    "name": "integration-test",
                    "version": "1.0.0"
                }
            }
        }))?;

        let response = self.recv()?;
        if response.get("result").is_none() {
            bail!("Initialize failed: {response:?}");
        }

        // Send initialized notification — this triggers the roots/list request
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))?;

        // The server should send us a roots/list request
        let roots_request = self.recv()?;
        let method = roots_request
            .get("method")
            .and_then(|m| m.as_str())
            .ok_or_else(|| anyhow!("Expected roots/list request, got: {roots_request:?}"))?;
        if method != "roots/list" {
            bail!("Expected roots/list, got {method}");
        }
        let request_id = roots_request
            .get("id")
            .ok_or_else(|| anyhow!("roots/list request missing id"))?
            .clone();

        // Respond with the provided roots
        let root_objects: Vec<Value> = roots
            .iter()
            .map(|r| json!({"uri": format!("file://{r}")}))
            .collect();

        self.send(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": { "roots": root_objects }
        }))?;

        // Small delay for processing
        std::thread::sleep(Duration::from_millis(100));
        Ok(())
    }

    /// Enters editing mode, accumulates a file, then calls `done_editing`
    /// via MCP to retrieve diagnostics from the tool result.
    fn call_diagnostics(&mut self, file: &str) -> Result<String> {
        let state_home = self
            .state_home
            .as_ref()
            .context("state_home not set")?
            .clone();
        let sessions_dir = PathBuf::from(&state_home).join("catenary").join("sessions");
        let socket_path = find_notify_socket(&sessions_dir)?;

        // Enter editing mode via IPC
        ipc_request(
            &socket_path,
            &json!({
                "method": "pre-tool/enforce-editing",
                "tool_name": "start_editing",
                "agent_id": ""
            }),
        )?;

        // Accumulate file via IPC
        ipc_request(
            &socket_path,
            &json!({
                "method": "post-tool/diagnostics",
                "file": file,
                "tool": "Edit",
                "agent_id": ""
            }),
        )?;

        // Call done_editing via MCP
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 9000,
            "method": "tools/call",
            "params": {
                "name": "done_editing",
                "arguments": {}
            }
        }))?;

        let response = self.recv()?;
        let text = response
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        Ok(text)
    }
}

/// Sends a one-shot IPC request to the hook server. Ignores the response.
fn ipc_request(socket_path: &std::path::Path, request: &Value) -> Result<()> {
    use std::io::Read as _;
    let mut stream =
        std::os::unix::net::UnixStream::connect(socket_path).context("connect to notify socket")?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(stream, "{request}").context("write to notify socket")?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    Ok(())
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
fn test_mcp_initialize() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], dir.path().to_str().context("dir")?)?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "test-client",
                "version": "1.0.0"
            }
        }
    }))?;

    let response = bridge.recv()?;

    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert!(response.get("result").is_some());

    let result = &response["result"];
    assert_eq!(result["protocolVersion"], "2024-11-05");
    assert_eq!(result["serverInfo"]["name"], "catenary");
    assert!(result["capabilities"]["tools"].is_object());
    Ok(())
}

#[test]
fn test_mcp_tools_list() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], dir.path().to_str().context("dir")?)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list"
    }))?;

    let response = bridge.recv()?;

    assert!(response.get("result").is_some());
    let tools = response["result"]["tools"]
        .as_array()
        .context("Missing tools array")?;

    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // Check all expected tools are present (2 after diagnostics removal)
    let expected_tools = ["grep", "glob"];

    for expected in &expected_tools {
        assert!(tool_names.contains(expected), "Missing {expected} tool");
    }

    // Verify all tools have valid schemas
    for tool in tools {
        let name = tool["name"].as_str().context("Missing tool name")?;
        assert!(
            tool.get("inputSchema").is_some(),
            "Tool {name} missing inputSchema"
        );
        let schema = &tool["inputSchema"];
        assert_eq!(
            schema["type"], "object",
            "Tool {name} schema type is not object"
        );
        assert!(
            schema["properties"].is_object(),
            "Tool {name} has no properties"
        );
    }
    Ok(())
}

#[test]
fn test_mcp_tool_call_unknown_tool() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], dir.path().to_str().context("dir")?)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "unknown_tool",
            "arguments": {}
        }
    }))?;

    let response = bridge.recv()?;

    assert!(response.get("result").is_some());

    let result = &response["result"];
    assert_eq!(result["isError"], true, "Expected error for unknown tool");
    Ok(())
}

#[test]
fn test_mcp_ping() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], dir.path().to_str().context("dir")?)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "ping"
    }))?;

    let response = bridge.recv()?;

    assert!(response.get("result").is_some());
    assert!(response.get("error").is_none());
    Ok(())
}

#[test]
fn test_client_info_stored_in_session() -> Result<()> {
    let state_dir = tempfile::tempdir().context("Failed to create state dir")?;
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");

    // Spawn bridge with isolated state dir so `catenary list` only sees this session
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.env("CATENARY_SERVERS", &lsp)
        .env("CATENARY_ROOTS", state_dir.path())
        .env("XDG_CONFIG_HOME", state_dir.path())
        .env("XDG_STATE_HOME", state_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context("Failed to spawn bridge")?;
    let stdin = child.stdin.take().context("Failed to get stdin")?;
    let stdout = BufReader::new(child.stdout.take().context("Failed to get stdout")?);
    let stderr = child.stderr.take();
    let mut bridge = BridgeProcess {
        child,
        stdin: Some(stdin),
        stdout: Some(stdout),
        stderr,
        state_home: Some(state_dir.path().to_string_lossy().into_owned()),
    };

    // Send initialize with specific client info
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "TestClient",
                "version": "42.0.0"
            }
        }
    }))?;

    let response = bridge.recv()?;
    assert!(response.get("result").is_some(), "Initialize failed");

    // Small delay to allow session update
    std::thread::sleep(Duration::from_millis(200));

    // Run catenary list with the same isolated state dir
    let output = Command::new(env!("CARGO_BIN_EXE_catenary"))
        .arg("list")
        .env("XDG_STATE_HOME", state_dir.path())
        .output()
        .context("Failed to run catenary list")?;

    let stdout_str = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout_str.contains("TestClient"),
        "Expected client info 'TestClient' in catenary list output, got:\n{stdout_str}"
    );
    Ok(())
}

#[test]
fn test_multi_root_find_symbol() -> Result<()> {
    // Create two roots with unique function names
    let dir_a = tempfile::tempdir().context("Failed to create temp dir A")?;
    let dir_b = tempfile::tempdir().context("Failed to create temp dir B")?;

    let script_a = dir_a.path().join(format!("alpha.{MOCK_LANG_A}"));
    std::fs::write(&script_a, "function alpha_func()\nalpha_func\n")?;

    let script_b = dir_b.path().join(format!("beta.{MOCK_LANG_A}"));
    std::fs::write(&script_b, "function beta_func()\nbeta_func\n")?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp], &[root_a, root_b])?;
    bridge.initialize()?;

    // Search should locate alpha_func from root A (via symbols or heatmap)
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 700,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "alpha_func" }
        }
    }))?;

    let response_a = bridge.recv()?;
    let result_a = &response_a["result"];
    assert!(
        result_a["isError"].is_null() || result_a["isError"] == false,
        "search for alpha_func failed: {response_a:?}"
    );
    let text_a = result_a["content"][0]["text"]
        .as_str()
        .context("Missing text for alpha_func")?;
    assert!(
        text_a.contains(&format!("alpha.{MOCK_LANG_A}")),
        "Expected search to find alpha.mock, got: {text_a}"
    );
    assert!(
        text_a.contains("# alpha_func"),
        "Expected symbol heading for alpha_func, got: {text_a}"
    );
    assert!(
        text_a.contains("[Function]"),
        "Expected [Function] kind for alpha_func, got: {text_a}"
    );

    // Search should locate beta_func from root B (via symbols or heatmap)
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 701,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "beta_func" }
        }
    }))?;

    let response_b = bridge.recv()?;
    let result_b = &response_b["result"];
    assert!(
        result_b["isError"].is_null() || result_b["isError"] == false,
        "search for beta_func failed: {response_b:?}"
    );
    let text_b = result_b["content"][0]["text"]
        .as_str()
        .context("Missing text for beta_func")?;
    assert!(
        text_b.contains(&format!("beta.{MOCK_LANG_A}")),
        "Expected search to find beta.mock, got: {text_b}"
    );

    Ok(())
}

#[test]
fn test_multi_root_glob_file() -> Result<()> {
    // Create two roots with different outline symbols
    let dir_a = tempfile::tempdir().context("Failed to create temp dir A")?;
    let dir_b = tempfile::tempdir().context("Failed to create temp dir B")?;

    let script_a = dir_a.path().join(format!("syms_a.{MOCK_LANG_A}"));
    std::fs::write(&script_a, "struct AlphaType\nenum BetaMode\n")?;

    let script_b = dir_b.path().join(format!("syms_b.{MOCK_LANG_A}"));
    std::fs::write(&script_b, "struct GammaType\nenum DeltaMode\n")?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp], &[root_a, root_b])?;
    bridge.initialize()?;

    // Get outline from root A file
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 720,
        "method": "tools/call",
        "params": {
            "name": "glob",
            "arguments": {
                "pattern": script_a.to_str().context("Invalid script A path")?
            }
        }
    }))?;

    let response_a = bridge.recv()?;
    let result_a = &response_a["result"];
    assert!(
        result_a["isError"].is_null() || result_a["isError"] == false,
        "Glob file from root A failed: {response_a:?}"
    );
    let text_a = result_a["content"][0]["text"]
        .as_str()
        .context("Missing text for symbols A")?;
    assert!(
        text_a.contains("AlphaType"),
        "Should contain AlphaType, got: {text_a}"
    );
    assert!(
        text_a.contains("BetaMode"),
        "Should contain BetaMode, got: {text_a}"
    );

    // Get outline from root B file
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 721,
        "method": "tools/call",
        "params": {
            "name": "glob",
            "arguments": {
                "pattern": script_b.to_str().context("Invalid script B path")?
            }
        }
    }))?;

    let response_b = bridge.recv()?;
    let result_b = &response_b["result"];
    assert!(
        result_b["isError"].is_null() || result_b["isError"] == false,
        "Glob file from root B failed: {response_b:?}"
    );
    let text_b = result_b["content"][0]["text"]
        .as_str()
        .context("Missing text for symbols B")?;
    assert!(
        text_b.contains("GammaType"),
        "Should contain GammaType, got: {text_b}"
    );
    assert!(
        text_b.contains("DeltaMode"),
        "Should contain DeltaMode, got: {text_b}"
    );

    Ok(())
}

// ─── sync_roots capability tests ────────────────────────────────────────

/// mockls without `--workspace-folders` does NOT support
/// `workspace/didChangeWorkspaceFolders`. When roots change, the server should
/// be shut down and lazily respawned with the updated root set on the next
/// query.
#[test]
fn test_sync_roots_restart_no_workspace_folders() -> Result<()> {
    let dir_a = tempfile::tempdir().context("Failed to create temp dir A")?;
    let dir_b = tempfile::tempdir().context("Failed to create temp dir B")?;

    let script_a = dir_a.path().join(format!("funcs_a.{MOCK_LANG_A}"));
    std::fs::write(
        &script_a,
        "function unique_root_a_func()\nunique_root_a_func\n",
    )?;

    let script_b = dir_b.path().join(format!("funcs_b.{MOCK_LANG_A}"));
    std::fs::write(
        &script_b,
        "function unique_root_b_func()\nunique_root_b_func\n",
    )?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    // Spawn bridge with only root_a
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root_a)?;
    bridge.initialize_with_roots(&[root_a])?;

    // Search in root_a — server should be working
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "unique_root_a_func" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Search in root A failed: {response:?}"
    );

    // Send roots/list_changed, respond with both roots
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/roots/list_changed"
    }))?;

    let roots_request = bridge.recv()?;
    let method = roots_request
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or_else(|| anyhow!("Expected roots/list request, got: {roots_request:?}"))?;
    assert_eq!(method, "roots/list");

    let request_id = roots_request
        .get("id")
        .ok_or_else(|| anyhow!("roots/list request missing id"))?
        .clone();

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": {
            "roots": [
                {"uri": format!("file://{root_a}")},
                {"uri": format!("file://{root_b}")}
            ]
        }
    }))?;

    // Search in root_b — server should have been restarted with new roots.
    // search waits for all servers to be ready, but retry to accommodate restart.
    let mut success = false;
    let mut last_text = String::new();
    for i in 0..10 {
        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": 20 + i,
            "method": "tools/call",
            "params": {
                "name": "grep",
                "arguments": { "pattern": "unique_root_b_func" }
            }
        }))?;

        let response = bridge.recv()?;
        let result = &response["result"];
        if result["isError"] == true {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        let text = result["content"][0]["text"]
            .as_str()
            .context("Missing text")?;
        last_text = text.to_string();
        if text.contains("## [") && text.contains(&format!("funcs_b.{MOCK_LANG_A}")) {
            success = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    assert!(
        success,
        "Search in root B should find ## [ with funcs_b.mock after server restart. Last output: {last_text}"
    );

    Ok(())
}

// ─── roots/list tests ───────────────────────────────────────────────────

#[test]
fn test_roots_list_after_initialize() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("dir")?;
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;

    // Initialize with roots capability — this validates the full round-trip:
    // initialize → notifications/initialized → server sends roots/list →
    // client responds → server applies roots
    bridge.initialize_with_roots(&[root])?;

    // Verify the server is still functional after roots exchange
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 100,
        "method": "ping"
    }))?;

    let response = bridge.recv()?;
    assert!(
        response.get("result").is_some(),
        "Ping should succeed after roots exchange"
    );

    Ok(())
}

#[test]
fn test_roots_list_changed_notification() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("dir")?;
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;

    // Initialize with roots capability
    bridge.initialize_with_roots(&[root])?;

    // Send roots/list_changed notification — server should send another roots/list request
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/roots/list_changed"
    }))?;

    // Read the roots/list request
    let roots_request = bridge.recv()?;
    let method = roots_request
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or_else(|| anyhow!("Expected roots/list request, got: {roots_request:?}"))?;
    assert_eq!(method, "roots/list", "Server should re-fetch roots");

    let request_id = roots_request
        .get("id")
        .ok_or_else(|| anyhow!("roots/list request missing id"))?
        .clone();

    // Respond with updated roots
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": {
            "roots": [
                {"uri": "file:///tmp", "name": "tmp"},
                {"uri": "file:///var", "name": "var"}
            ]
        }
    }))?;

    std::thread::sleep(Duration::from_millis(100));

    // Verify still functional
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 200,
        "method": "ping"
    }))?;

    let response = bridge.recv()?;
    assert!(
        response.get("result").is_some(),
        "Ping should succeed after roots update"
    );

    Ok(())
}

#[test]
fn test_no_roots_request_without_capability() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], dir.path().to_str().context("dir")?)?;

    // Initialize WITHOUT roots capability
    bridge.initialize()?;

    // Send a ping immediately — if the server had sent a roots/list request,
    // we'd read that instead of the ping response
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 300,
        "method": "ping"
    }))?;

    let response = bridge.recv()?;

    // This should be the ping response, not a roots/list request
    let id = response
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| anyhow!("Expected ping response, got: {response:?}"))?;
    assert_eq!(id, 300, "Should receive ping response, not roots/list");
    assert!(response.get("result").is_some());

    Ok(())
}

// ─── mockls-based tests ─────────────────────────────────────────────────
// These tests use mockls instead of real language servers, so they always
// run regardless of installed toolchains.

/// Build a `CATENARY_SERVERS` spec for `BridgeProcess::spawn` using mockls.
fn mockls_lsp_arg(lang: &str, flags: &str) -> String {
    let bin = env!("CARGO_BIN_EXE_mockls");
    if flags.is_empty() {
        format!("{lang}:{bin} {lang}")
    } else {
        format!("{lang}:{bin} {lang} {flags}")
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "Parameterized test iterates over profiles"
)]
fn test_mockls_sync_roots_across_profiles() -> Result<()> {
    let profiles: &[(&str, &str)] = &[
        ("no-workspace-folders", "--scan-roots"),
        ("workspace-folders", "--workspace-folders --scan-roots"),
    ];

    for (name, flags) in profiles {
        let dir_a = tempfile::tempdir().context("Failed to create temp dir A")?;
        let dir_b = tempfile::tempdir().context("Failed to create temp dir B")?;

        let script_a = dir_a.path().join(format!("funcs_a.{MOCK_LANG_A}"));
        std::fs::write(&script_a, "fn unique_root_a_func()\nunique_root_a_func\n")?;

        let script_b = dir_b.path().join(format!("funcs_b.{MOCK_LANG_A}"));
        std::fs::write(&script_b, "fn unique_root_b_func()\nunique_root_b_func\n")?;

        let root_a = dir_a.path().to_str().context("Invalid path A")?;
        let root_b = dir_b.path().to_str().context("Invalid path B")?;

        let lsp = mockls_lsp_arg(MOCK_LANG_A, flags);
        let mut bridge = BridgeProcess::spawn(&[&lsp], root_a)?;
        bridge.initialize_with_roots(&[root_a])?;

        // Search in root_a — server should be working
        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "grep",
                "arguments": { "pattern": "unique_root_a_func" }
            }
        }))?;

        let response = bridge.recv()?;
        let result = &response["result"];
        assert!(
            result["isError"].is_null() || result["isError"] == false,
            "Profile {name}: search in root A failed: {response:?}"
        );

        // Send roots/list_changed with both roots
        bridge.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/roots/list_changed"
        }))?;

        let roots_request = bridge.recv()?;
        let method = roots_request
            .get("method")
            .and_then(|m| m.as_str())
            .ok_or_else(|| {
                anyhow!("Profile {name}: Expected roots/list, got: {roots_request:?}")
            })?;
        assert_eq!(method, "roots/list");

        let request_id = roots_request
            .get("id")
            .ok_or_else(|| anyhow!("Profile {name}: roots/list missing id"))?
            .clone();

        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "roots": [
                    {"uri": format!("file://{root_a}")},
                    {"uri": format!("file://{root_b}")}
                ]
            }
        }))?;

        // Search in root_b — bridge waits for all servers to be ready
        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "tools/call",
            "params": {
                "name": "grep",
                "arguments": { "pattern": "unique_root_b_func" }
            }
        }))?;

        let response = bridge.recv()?;
        let result = &response["result"];
        assert!(
            result["isError"] != true,
            "Profile {name}: search in root B returned error: {response:?}"
        );
        let text = result["content"][0]["text"]
            .as_str()
            .context("Missing text")?;
        assert!(
            text.contains(&format!("funcs_b.{MOCK_LANG_A}")),
            "Profile {name}: search in root B should reference funcs_b.mock, got: {text}"
        );
        assert!(
            text.contains("## ["),
            "Profile {name}: search in root B should have Symbols section, got: {text}"
        );
    }
    Ok(())
}

/// Verifies that a server supporting workspace folders but not `$/progress`
/// doesn't hang after a root is added. The `wait_ready()` activity settle
/// fallback must transition the server back to `Ready`.
#[test]
fn test_mockls_sync_roots_no_progress_no_hang() -> Result<()> {
    let dir_a = tempfile::tempdir().context("Failed to create temp dir A")?;
    let dir_b = tempfile::tempdir().context("Failed to create temp dir B")?;

    let file_a = dir_a.path().join(format!("funcs_a.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn hello()\nhello\n")?;
    let file_b = dir_b.path().join(format!("funcs_b.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn world()\nworld\n")?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    // mockls with --workspace-folders and --scan-roots but NO --indexing-delay:
    // supports didChangeWorkspaceFolders, never sends $/progress.
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--workspace-folders --scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root_a)?;
    bridge.initialize_with_roots(&[root_a])?;

    // Search in root_a — establishes server is working
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "hello" }
        }
    }))?;

    let response = bridge.recv()?;
    assert!(
        response["result"]["isError"] != true,
        "Root A search failed: {response:?}"
    );

    // Add root_b via roots/list_changed
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/roots/list_changed"
    }))?;

    let roots_request = bridge.recv()?;
    assert_eq!(
        roots_request["method"], "roots/list",
        "Expected roots/list request, got: {roots_request:?}"
    );

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": roots_request["id"],
        "result": {
            "roots": [
                {"uri": format!("file://{root_a}")},
                {"uri": format!("file://{root_b}")}
            ]
        }
    }))?;

    // Search in root_b — must not hang.
    // did_change_workspace_folders sets state to Busy.
    // Since mockls never sends $/progress, wait_ready() uses
    // the activity settle fallback to transition back to Ready.
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 20,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "world" }
        }
    }))?;

    let response = bridge.recv()?;
    assert!(
        response["result"]["isError"] != true,
        "Root B search should not hang or error: {response:?}"
    );
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing search text for root B")?;
    assert!(
        text.contains(&format!("funcs_b.{MOCK_LANG_A}")),
        "Expected 'funcs_b.mock' in search results, got: {text}"
    );
    assert!(
        text.contains("## ["),
        "Root B search should have Symbols section, got: {text}"
    );

    Ok(())
}

#[test]
fn test_mockls_multiplexing() -> Result<()> {
    // Spawn two mockls instances as different languages
    let dir = tempfile::tempdir()?;

    let shell_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&shell_file, "fn greet()\ngreet\n")?;

    let second_file = dir.path().join(format!("test.{MOCK_LANG_B}"));
    std::fs::write(&second_file, "[package]\nname = \"test\"\n")?;

    let lsp_shell = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let lsp_second = mockls_lsp_arg(MOCK_LANG_B, "");
    let root = dir.path().to_str().context("Invalid root path")?;

    let mut bridge = BridgeProcess::spawn(&[&lsp_shell, &lsp_second], root)?;
    bridge.initialize()?;

    // Search for "greet" — should find in MOCK_LANG_A file
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 100,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "greet" }
        }
    }))?;

    let response_a = bridge.recv()?;
    let result_a = &response_a["result"];
    assert!(
        result_a["isError"].is_null() || result_a["isError"] == false,
        "Lang A search failed: {response_a:?}"
    );

    // Search for "package" — should find in MOCK_LANG_B file
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 101,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "package" }
        }
    }))?;

    let response_b = bridge.recv()?;
    let result_b = &response_b["result"];
    assert!(
        result_b["isError"].is_null() || result_b["isError"] == false,
        "Lang B search failed: {response_b:?}"
    );

    let text_a = result_a["content"][0]["text"]
        .as_str()
        .context("Missing lang A search text")?;
    let text_b = result_b["content"][0]["text"]
        .as_str()
        .context("Missing lang B search text")?;

    assert!(
        text_a.contains(&format!("test.{MOCK_LANG_A}")),
        "Lang A search should reference test file, got: {text_a}"
    );
    assert!(
        text_a.contains("## ["),
        "Lang A search should have Symbols section, got: {text_a}"
    );
    assert!(
        text_b.contains(&format!("test.{MOCK_LANG_B}")),
        "Lang B search should reference test file, got: {text_b}"
    );

    Ok(())
}

/// Verifies that Catenary does NOT send `didSave` when the server does not
/// advertise `textDocumentSync.save` (Gap 2 negative case).
#[test]
fn test_mockls_did_save_not_sent_without_capability() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    let test_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "echo hello\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--publish-version --notification-log {log_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Notify via socket — this triggers didOpen + (possibly) didSave
    let _ = bridge.call_diagnostics(test_file.to_str().context("file path")?)?;

    // Shut down to flush the log
    drop(bridge);
    std::thread::sleep(Duration::from_millis(200));

    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !log.contains("textDocument/didSave"),
        "didSave should NOT be sent without save capability. Log:\n{log}"
    );

    Ok(())
}

/// Verifies that Catenary DOES send `didSave` when the server advertises
/// `textDocumentSync.save` (Gap 2 positive case).
#[test]
fn test_mockls_did_save_sent_with_capability() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    let test_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "echo hello\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--publish-version --advertise-save --notification-log {log_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let _ = bridge.call_diagnostics(test_file.to_str().context("file path")?)?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(200));

    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log.contains("textDocument/didSave"),
        "didSave SHOULD be sent with save capability. Log:\n{log}"
    );

    Ok(())
}

/// Verifies that search degrades gracefully when LSP methods fail.
/// `--fail-on workspace/symbol` makes workspace/symbol return `InternalError`.
/// Search should still return ripgrep file matches, and the rg-bootstrapped
/// enrichment path should recover symbols via hover even when the symbol
/// universe is unavailable.
#[test]
fn test_search_graceful_degradation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let test_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn greet()\ngreet\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots --fail-on workspace/symbol");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "greet" }
        }
    }))?;

    let response = bridge.recv()?;
    assert!(
        response["result"]["isError"] != true,
        "Search should succeed even when workspace/symbol fails: {response:?}"
    );
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing search text")?;
    // Should still find via ripgrep
    assert!(
        text.contains(&format!("test.{MOCK_LANG_A}")),
        "Search should find test.mock via ripgrep, got: {text}"
    );
    // Rg-bootstrapped enrichment recovers the symbol via hover even
    // when workspace/symbol fails.
    assert!(
        text.contains("## ["),
        "Bootstrap should recover symbol via hover when workspace/symbol fails, got: {text}"
    );

    Ok(())
}

/// Verifies that a server burning CPU after a workspace folder change
/// does not block `wait_ready` — lifecycle-based readiness returns
/// immediately since the server is already `Healthy`.
///
/// mockls `--cpu-on-workspace-change 15000` burns 15s of CPU on
/// `workspace/didChangeWorkspaceFolders`. The server is already `Healthy`
/// (init completed), so `wait_ready` returns `true` immediately.
/// Individual LSP requests may time out via `Connection::request`'s
/// failure detection, but grep degrades gracefully via ripgrep.
#[test]
fn test_wait_ready_failure_detection() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let test_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "echo hello\n")?;

    let dir2 = tempfile::tempdir()?;

    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        "--workspace-folders --cpu-on-workspace-change 15000",
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize_with_roots(&[root])?;

    // Send roots/list_changed notification to trigger workspace folder change
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/roots/list_changed"
    }))?;

    // Server sends roots/list request — respond with both roots
    let roots_request = bridge.recv()?;
    let method = roots_request
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or_else(|| anyhow!("Expected roots/list request, got: {roots_request:?}"))?;
    if method != "roots/list" {
        bail!("Expected roots/list, got {method}");
    }
    let request_id = roots_request
        .get("id")
        .ok_or_else(|| anyhow!("roots/list request missing id"))?
        .clone();

    let root2 = dir2.path().to_str().context("root2 path")?;
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": {
            "roots": [
                {"uri": format!("file://{root}")},
                {"uri": format!("file://{root2}")}
            ]
        }
    }))?;

    // Small delay for the workspace folder change to be sent to mockls
    std::thread::sleep(Duration::from_millis(200));

    // Send a search request — wait_ready returns true (server is Healthy),
    // but individual LSP requests may time out during the CPU burn.
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "hello" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];

    // Search degrades gracefully — ripgrep results still present
    assert!(
        result.get("isError").is_none() || result["isError"] == false,
        "Search should degrade gracefully, not error. Got: {response:?}"
    );

    let content = result["content"]
        .as_array()
        .context("Missing content array")?;
    let text = content[0]["text"].as_str().context("Missing result text")?;
    assert!(
        text.contains("hello"),
        "Ripgrep results should still contain the match. Got: {text}"
    );

    Ok(())
}

/// Verifies that a server burning CPU on `initialized` does not prevent
/// search from succeeding.
///
/// mockls `--cpu-on-initialized 3000` burns 3s of CPU on `initialized`.
/// The server is set to `Healthy` after init completes, so `wait_ready`
/// returns `true` immediately. The search request succeeds because the
/// server responds to requests after the CPU burn finishes.
#[test]
fn test_warmup_observation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let test_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn my_function()\nmy_function\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots --cpu-on-initialized 3000");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Send search immediately — server is still burning CPU
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "my_function" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    // Should succeed — wait_ready waits for CPU burn to finish
    let text = result["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains(&format!("test.{MOCK_LANG_A}")),
        "Search should succeed after warmup observation. Got: {text}"
    );
    assert!(
        text.contains("## ["),
        "Search after warmup should have Symbols section, got: {text}"
    );

    Ok(())
}

// ─── scan-roots and enrichment tests ─────────────────────────────────────

/// Verifies that `--scan-roots` makes workspace symbols available without
/// a prior `didOpen`. Without `--scan-roots`, search only finds text via
/// ripgrep; with it, LSP workspace symbols appear in the `## Symbols` section.
#[test]
fn test_search_symbols_with_scan_roots() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let test_file = dir.path().join(format!("greeter.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn greet()\ngreet\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "greet" }
        }
    }))?;

    let response = bridge.recv()?;
    assert!(
        response["result"]["isError"] != true,
        "Search should succeed: {response:?}"
    );
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing search text")?;

    assert!(
        text.contains("## ["),
        "Search with --scan-roots should produce ## Symbols section, got: {text}"
    );
    assert!(
        text.contains("greet"),
        "Symbols section should contain 'greet', got: {text}"
    );

    Ok(())
}

/// Verifies per-symbol `#` / `##` output structure with enrichment.
/// Two symbols, each with hover and references.
#[test]
fn test_grep_per_symbol_output() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let file_a = dir.path().join(format!("mod_a.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn load_config()\nload_config\n")?;

    let file_b = dir.path().join(format!("mod_b.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn save_config()\nsave_config\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "load_config|save_config" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Per-symbol # headings
    assert!(
        text.contains("# load_config") || text.contains("# save_config"),
        "Expected # symbol headings, got:\n{text}"
    );

    // ## [Kind] definition headings
    assert!(
        text.contains("## [Function]"),
        "Expected ## [Function] headings, got:\n{text}"
    );

    // Hover content (code block from mockls)
    assert!(
        text.contains("```"),
        "Expected hover code block in enriched output, got:\n{text}"
    );

    Ok(())
}

/// Verifies that mockls with `--resolve-provider` returns URI-only symbols
/// that are resolved via `workspaceSymbol/resolve`, producing the same output.
#[test]
fn test_grep_resolve_provider() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let test_file = dir.path().join(format!("resolve.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn resolve_me()\nresolve_me\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots --resolve-provider");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "resolve_me" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Symbol should be found and resolved
    assert!(
        text.contains("# resolve_me"),
        "Expected # resolve_me heading, got:\n{text}"
    );
    assert!(
        text.contains("## [Function]"),
        "Expected ## [Function] heading after resolve, got:\n{text}"
    );
    assert!(
        text.contains(&format!("resolve.{MOCK_LANG_A}")),
        "Expected file name in output, got:\n{text}"
    );

    Ok(())
}

/// Verifies that pipe-separated alternation finds symbols from both patterns.
/// `pattern: "alpha_func|beta_func"` should find both across two roots.
#[test]
fn test_grep_alternation() -> Result<()> {
    let dir_a = tempfile::tempdir().context("Failed to create temp dir A")?;
    let dir_b = tempfile::tempdir().context("Failed to create temp dir B")?;

    let script_a = dir_a.path().join(format!("alpha.{MOCK_LANG_A}"));
    std::fs::write(&script_a, "function alpha_func()\nalpha_func\n")?;

    let script_b = dir_b.path().join(format!("beta.{MOCK_LANG_A}"));
    std::fs::write(&script_b, "function beta_func()\nbeta_func\n")?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp], &[root_a, root_b])?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 800,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "alpha_func|beta_func" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep alternation failed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text for alternation")?;

    // Both files should appear
    assert!(
        text.contains(&format!("alpha.{MOCK_LANG_A}")),
        "Expected alpha.mock in alternation results, got: {text}"
    );
    assert!(
        text.contains(&format!("beta.{MOCK_LANG_A}")),
        "Expected beta.mock in alternation results, got: {text}"
    );

    // Both symbols should appear
    assert!(
        text.contains("## ["),
        "Expected Symbols section for alternation, got: {text}"
    );
    assert!(
        text.contains("alpha_func"),
        "Expected alpha_func symbol, got: {text}"
    );
    assert!(
        text.contains("beta_func"),
        "Expected beta_func symbol, got: {text}"
    );

    Ok(())
}

/// Verifies that >10 unique symbols skips enrichment: no References section,
/// no hover content, but Symbols with name + kind + location are present.
#[test]
fn test_grep_enrichment_threshold_broad() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Create 11 unique functions to exceed GREP_ENRICHMENT_THRESHOLD (10)
    let test_file = dir.path().join(format!("many.{MOCK_LANG_A}"));
    std::fs::write(
        &test_file,
        "fn zz_broad_one()\nfn zz_broad_two()\nfn zz_broad_three()\n\
         fn zz_broad_four()\nfn zz_broad_five()\nfn zz_broad_six()\n\
         fn zz_broad_seven()\nfn zz_broad_eight()\nfn zz_broad_nine()\n\
         fn zz_broad_ten()\nfn zz_broad_eleven()\n\
         zz_broad_one\nzz_broad_two\nzz_broad_three\n\
         zz_broad_four\nzz_broad_five\nzz_broad_six\n\
         zz_broad_seven\nzz_broad_eight\nzz_broad_nine\n\
         zz_broad_ten\nzz_broad_eleven\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 810,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "zz_broad_one|zz_broad_two|zz_broad_three|zz_broad_four|zz_broad_five|zz_broad_six|zz_broad_seven|zz_broad_eight|zz_broad_nine|zz_broad_ten|zz_broad_eleven" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep broad should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text for broad search")?;

    // Symbols tier should be present with names and kinds
    assert!(
        text.contains("## ["),
        "Expected Symbols section for broad search, got: {text}"
    );
    assert!(
        text.contains("[Function]"),
        "Expected [Function] kind in broad search, got: {text}"
    );

    // Structural enrichment (callers, references) is always present
    assert!(
        text.contains("### Callers") || text.contains("### References"),
        "Structural enrichment should be present even above hover threshold, got: {text}"
    );

    // Hover content should NOT be present when >10 symbols
    assert!(
        !text.contains("> ```"),
        "Hover blockquote should not appear above hover threshold, got: {text}"
    );

    Ok(())
}

/// Test A — rg-only groups by matched string.
///
/// Files with no symbol definitions (plain text without `fn`/`function`/etc.)
/// should be grouped under `# matched_text` headings when queried with alternation.
#[test]
fn test_grep_rg_only_groups_by_matched_string() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Create files with no symbol definitions — just plain text
    let file_a = dir.path().join(format!("notes.{MOCK_LANG_A}"));
    std::fs::write(
        &file_a,
        "the alpha_token is important\nalpha_token appears again\n",
    )?;

    let file_b = dir.path().join(format!("readme.{MOCK_LANG_A}"));
    std::fs::write(
        &file_b,
        "beta_token is used here\nbeta_token is also here\n",
    )?;

    // Use --scan-roots so mockls indexes files, but no fn/struct/etc. definitions exist
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 900,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "alpha_token|beta_token" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep rg-only failed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Should have separate # headings for each matched string
    assert!(
        text.contains("# alpha_token"),
        "Expected '# alpha_token' heading, got:\n{text}"
    );
    assert!(
        text.contains("# beta_token"),
        "Expected '# beta_token' heading, got:\n{text}"
    );

    // Each heading should have its own file hits, not all dumped together
    assert!(
        text.contains(&format!("notes.{MOCK_LANG_A}")),
        "Expected notes file in alpha_token section, got:\n{text}"
    );
    assert!(
        text.contains(&format!("readme.{MOCK_LANG_A}")),
        "Expected readme file in beta_token section, got:\n{text}"
    );

    Ok(())
}

/// Test B — alternation routes non-code hits correctly.
///
/// Files with symbol definitions AND non-code mentions should have each `#`
/// heading receive the correct rg hits, not all dumped under the first one.
#[test]
fn test_grep_alternation_routes_non_code_hits() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // File with a symbol definition for "compute" and a plain mention of "render"
    let file_a = dir.path().join(format!("engine.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn compute()\ncompute\nrender is mentioned here\n")?;

    // File with a symbol definition for "render" and a plain mention of "compute"
    let file_b = dir.path().join(format!("display.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn render()\nrender\ncompute is mentioned here\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 910,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "compute|render" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep alternation routing failed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Both symbols should have # headings
    assert!(
        text.contains("# compute"),
        "Expected '# compute' heading, got:\n{text}"
    );
    assert!(
        text.contains("# render"),
        "Expected '# render' heading, got:\n{text}"
    );

    // Both should have ## [Function] definition sub-headings
    let compute_idx = text
        .find("# compute")
        .context("Missing # compute heading")?;
    let render_idx = text.find("# render").context("Missing # render heading")?;

    // Each section should contain its own ## [Function] heading
    let (first_section, second_section) = if compute_idx < render_idx {
        (&text[compute_idx..render_idx], &text[render_idx..])
    } else {
        (&text[render_idx..compute_idx], &text[compute_idx..])
    };

    assert!(
        first_section.contains("## [Function]"),
        "First section should have ## [Function], got:\n{first_section}"
    );
    assert!(
        second_section.contains("## [Function]"),
        "Second section should have ## [Function], got:\n{second_section}"
    );

    Ok(())
}

/// Test C — two definitions under one `#` heading with per-`##` references.
///
/// Two files each defining the same function name should produce one `#`
/// heading with two `##` sub-headings, each showing their own references.
#[test]
fn test_grep_two_defs_same_name_per_heading_refs() -> Result<()> {
    let dir_a = tempfile::tempdir()?;
    let dir_b = tempfile::tempdir()?;

    let file_a = dir_a.path().join(format!("impl_a.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn process()\nprocess\n")?;

    let file_b = dir_b.path().join(format!("impl_b.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn process()\nprocess\n")?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp], &[root_a, root_b])?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 920,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "process" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep two-defs failed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Should have exactly one # heading
    assert!(
        text.contains("# process"),
        "Expected '# process' heading, got:\n{text}"
    );

    // Count ## [Function] headings — should be exactly 2
    let def_count = text.matches("## [Function]").count();
    assert_eq!(
        def_count, 2,
        "Expected 2 ## [Function] headings for two defs, got {def_count}:\n{text}"
    );

    // Both files should appear
    assert!(
        text.contains(&format!("impl_a.{MOCK_LANG_A}")),
        "Expected impl_a in output, got:\n{text}"
    );
    assert!(
        text.contains(&format!("impl_b.{MOCK_LANG_A}")),
        "Expected impl_b in output, got:\n{text}"
    );

    Ok(())
}

/// Verifies that `fetch_symbols_by_queries` handles `OneOf::Right` (URI-only)
/// symbols via resolve. Uses `--no-empty-query` to force the fallback path
/// (universe returns empty, per-query lookup fires) combined with
/// `--resolve-provider` so per-query results need resolve.
#[test]
fn test_grep_resolve_fallback_path() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let test_file = dir.path().join(format!("fallback.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn resolve_fallback()\nresolve_fallback\n")?;

    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        "--scan-roots --resolve-provider --no-empty-query",
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 930,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "resolve_fallback" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep resolve fallback failed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Symbol should be found via fallback + resolve
    assert!(
        text.contains("# resolve_fallback"),
        "Expected '# resolve_fallback' heading, got:\n{text}"
    );
    assert!(
        text.contains("## [Function]"),
        "Expected ## [Function] heading after fallback resolve, got:\n{text}"
    );
    assert!(
        text.contains(&format!("fallback.{MOCK_LANG_A}")),
        "Expected file name in output, got:\n{text}"
    );

    Ok(())
}

/// Verifies that the same symbol name found by two different language servers
/// produces a single `#` heading with `##` sub-headings from each server.
#[test]
fn test_grep_cross_server_same_symbol() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let file_a = dir.path().join(format!("shared.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn cross_server_fn()\ncross_server_fn\n")?;

    let file_b = dir.path().join(format!("shared.{MOCK_LANG_B}"));
    std::fs::write(&file_b, "fn cross_server_fn()\ncross_server_fn\n")?;

    let lsp_a = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let lsp_b = mockls_lsp_arg(MOCK_LANG_B, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;

    let mut bridge = BridgeProcess::spawn(&[&lsp_a, &lsp_b], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 940,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "cross_server_fn" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep cross-server failed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Single # heading
    assert!(
        text.contains("# cross_server_fn"),
        "Expected '# cross_server_fn' heading, got:\n{text}"
    );

    // Two ## [Function] sub-headings, one from each server's file
    let def_count = text.matches("## [Function]").count();
    assert_eq!(
        def_count, 2,
        "Expected 2 ## [Function] headings from two servers, got {def_count}:\n{text}"
    );

    // Both files should appear
    assert!(
        text.contains(&format!("shared.{MOCK_LANG_A}")),
        "Expected shared.{MOCK_LANG_A} in output, got:\n{text}"
    );
    assert!(
        text.contains(&format!("shared.{MOCK_LANG_B}")),
        "Expected shared.{MOCK_LANG_B} in output, got:\n{text}"
    );

    Ok(())
}

/// Verifies that enriched output for functions includes a "Called by:" section
/// listing the enclosing caller name.
#[test]
fn test_grep_enrichment_incoming_calls() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let test_file = dir.path().join(format!("calls.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn callee_fn()\nfn caller_fn()\n  callee_fn\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 950,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "callee_fn" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep incoming calls should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text for incoming calls")?;

    assert!(
        text.contains("### Callers"),
        "Expected '### Callers' section, got:\n{text}"
    );
    assert!(
        text.contains("caller_fn"),
        "Expected caller_fn in '### Callers' section, got:\n{text}"
    );

    Ok(())
}

/// Verifies that enriched output for structs includes an "Implementations:" section.
#[test]
fn test_grep_enrichment_implementations() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // mockls routes textDocument/implementation to handle_references,
    // so any reference location will appear as an implementation entry.
    let test_file = dir.path().join(format!("impls.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "struct MyStruct\nMyStruct\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 960,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "MyStruct" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep implementations should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text for implementations")?;

    assert!(
        text.contains("### Implementations"),
        "Expected '### Implementations' section, got:\n{text}"
    );

    Ok(())
}

/// Verifies that enriched output for interfaces includes a "Subtypes:" section.
#[test]
fn test_grep_enrichment_subtypes() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let test_file = dir.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "interface Animal\nstruct Dog\nclass Cat\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 970,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "Animal" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep subtypes should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text for subtypes")?;

    assert!(
        text.contains("### Subtypes"),
        "Expected '### Subtypes' section, got:\n{text}"
    );
    assert!(
        text.contains("Dog"),
        "Expected Dog in '### Subtypes' section, got:\n{text}"
    );
    assert!(
        text.contains("Cat"),
        "Expected Cat in '### Subtypes' section, got:\n{text}"
    );

    Ok(())
}

/// Diagnostic test: does the rg bootstrap path recover methods that
/// `workspace/symbol("")` from rust-analyzer truncates?
///
/// Creates a small Rust workspace with a struct + method, spawns the bridge
/// with real rust-analyzer, and greps for the method name. The symbol
/// universe typically truncates impl methods, but the rg-bootstrapped
/// enrichment should recover them via hover at the rg hit position,
/// producing a `## [Function]` or `## [Method]` heading with enrichment.
///
/// Run with: `make test T=ra_symbol_universe`
/// Requires: rust-analyzer on PATH.
#[test]
#[ignore = "requires rust-analyzer; diagnostic test for symbol universe coverage"]
fn test_ra_symbol_universe_includes_methods() -> Result<()> {
    use std::io::Write as _;

    let dir = tempfile::tempdir()?;

    // Minimal Cargo.toml so rust-analyzer treats this as a real project
    let cargo_toml = dir.path().join("Cargo.toml");
    std::fs::write(
        &cargo_toml,
        "[package]\nname = \"ra-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )?;

    let src_dir = dir.path().join("src");
    std::fs::create_dir(&src_dir)?;
    let lib_rs = src_dir.join("lib.rs");
    std::fs::write(
        &lib_rs,
        "pub struct ZzTestWidget {\n    value: u32,\n}\n\n\
         impl ZzTestWidget {\n    \
             pub fn zz_widget_method(&self) -> u32 {\n        \
                 self.value\n    \
             }\n\
         }\n",
    )?;

    let lsp_arg = "rust:rust-analyzer".to_string();
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp_arg], root)?;
    bridge.initialize()?;

    // rust-analyzer needs time to index; poll with short sleeps
    let mut text = String::new();
    for attempt in 0..30 {
        std::thread::sleep(Duration::from_secs(2));

        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": 5000 + attempt,
            "method": "tools/call",
            "params": {
                "name": "grep",
                "arguments": { "pattern": "zz_widget_method" }
            }
        }))?;

        let response = bridge.recv()?;
        let result = &response["result"];
        if let Some(t) = result["content"][0]["text"].as_str() {
            if t.contains("## [") {
                // Got enriched symbol output — universe includes methods
                text = t.to_string();
                break;
            }
            if t.contains("zz_widget_method") {
                // Got rg hits but no symbol enrichment — keep trying
                // (RA may still be indexing)
                text = t.to_string();
            }
        }
    }

    // Flush stderr so we can see the output in test logs
    let _ = writeln!(
        std::io::stderr(),
        "\n=== ra_symbol_universe output ===\n{text}\n=== end ==="
    );

    assert!(
        text.contains("# zz_widget_method"),
        "Expected '# zz_widget_method' heading, got:\n{text}"
    );

    // This is the key assertion: the rg bootstrap should recover the method
    // that workspace/symbol("") truncated, producing an enriched heading.
    assert!(
        text.contains("## ["),
        "Expected ## [Function] or ## [Method] heading — \
         rg bootstrap should have recovered the truncated method. Got:\n{text}"
    );

    Ok(())
}

// ─── Rg-bootstrapped enrichment tests ───────────────────────────────────

/// Truncation bootstrap: when `--symbol-limit` causes a symbol to be missing
/// from the universe, the rg bootstrap path should recover it via hover.
#[test]
fn test_grep_rg_bootstrap_truncation() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Create three files with distinct function names. With --symbol-limit 1,
    // only the first one (alphabetically by URI) enters the universe.
    let file_a = dir.path().join(format!("aaa_first.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn alpha_sym()\nalpha_sym\n")?;

    let file_b = dir.path().join(format!("bbb_second.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn beta_sym()\nbeta_sym\n")?;

    let file_c = dir.path().join(format!("ccc_third.{MOCK_LANG_A}"));
    std::fs::write(&file_c, "fn gamma_sym()\ngamma_sym\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots --symbol-limit 1");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Grep for a symbol that is NOT in the top 1 of the universe
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 2000,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "gamma_sym" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep bootstrap truncation failed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text for bootstrap truncation")?;

    // Should have a # heading
    assert!(
        text.contains("# gamma_sym"),
        "Expected '# gamma_sym' heading, got:\n{text}"
    );

    // Key assertion: bootstrap should produce ## [Function] with hover enrichment
    assert!(
        text.contains("## [Function]"),
        "Expected ## [Function] heading via rg bootstrap, got:\n{text}"
    );
    assert!(
        text.contains("```"),
        "Expected hover code block via rg bootstrap, got:\n{text}"
    );

    Ok(())
}

/// Keyword prefix: grepping for `fn my_func` should still produce enrichment
/// because hover skips the keyword and resolves the actual symbol name.
#[test]
fn test_grep_rg_bootstrap_keyword_prefix() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let file = dir.path().join(format!("kw_test.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn kw_target_func()\nkw_target_func\n")?;

    // --symbol-limit 0 forces empty universe, so all enrichment comes from bootstrap
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots --symbol-limit 0");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 2010,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "fn kw_target_func" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep bootstrap keyword prefix failed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text for keyword prefix")?;

    // The keyword `fn` should be skipped by hover; `kw_target_func` should be enriched
    assert!(
        text.contains("## [Function]"),
        "Expected ## [Function] heading via keyword prefix bootstrap, got:\n{text}"
    );
    assert!(
        text.contains("```"),
        "Expected hover code block for keyword prefix, got:\n{text}"
    );

    Ok(())
}

/// Same-name disambiguation: two files each defining `fn process()` should
/// both get separate `## [Function]` headings via rg bootstrap.
#[test]
fn test_grep_rg_bootstrap_same_name() -> Result<()> {
    let dir_a = tempfile::tempdir()?;
    let dir_b = tempfile::tempdir()?;

    let file_a = dir_a.path().join(format!("impl_x.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn disambig_proc()\ndisambig_proc\n")?;

    let file_b = dir_b.path().join(format!("impl_y.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn disambig_proc()\ndisambig_proc\n")?;

    let root_a = dir_a.path().to_str().context("root A")?;
    let root_b = dir_b.path().to_str().context("root B")?;

    // --symbol-limit 0 forces all enrichment through bootstrap
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots --symbol-limit 0");
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp], &[root_a, root_b])?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 2020,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "disambig_proc" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep bootstrap same-name failed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text for same-name")?;

    // Should have one # heading
    assert!(
        text.contains("# disambig_proc"),
        "Expected '# disambig_proc' heading, got:\n{text}"
    );

    // Both files should appear with ## [Function] headings
    let def_count = text.matches("## [Function]").count();
    assert_eq!(
        def_count, 2,
        "Expected 2 ## [Function] headings for same-name disambiguation, got {def_count}:\n{text}"
    );

    assert!(
        text.contains(&format!("impl_x.{MOCK_LANG_A}")),
        "Expected impl_x in output, got:\n{text}"
    );
    assert!(
        text.contains(&format!("impl_y.{MOCK_LANG_A}")),
        "Expected impl_y in output, got:\n{text}"
    );

    Ok(())
}

/// Plain text: rg hits where hover returns nothing should produce rg-only
/// output (path + line ranges) with no `## [kind]` heading.
/// Uses `--fail-on textDocument/hover` so hover always fails, simulating
/// a position with no symbol.
#[test]
fn test_grep_rg_bootstrap_plain_text() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let file = dir.path().join(format!("notes.{MOCK_LANG_A}"));
    std::fs::write(&file, "plain_marker\nplain_marker\n")?;

    // --fail-on textDocument/prepareRename makes the keyword filter treat
    // every token as non-symbol, so the bootstrap produces rg-only output.
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        "--scan-roots --symbol-limit 0 --fail-on textDocument/prepareRename",
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 2030,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "plain_marker" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep bootstrap plain text failed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text for plain text")?;

    // Should have rg-only output with the heading
    assert!(
        text.contains("# plain_marker"),
        "Expected '# plain_marker' heading, got:\n{text}"
    );

    // Should NOT have any ## [kind] heading — prepareRename always failed
    assert!(
        !text.contains("## ["),
        "Expected no ## [kind] heading when prepareRename fails, got:\n{text}"
    );

    Ok(())
}

/// Regression: rg-bootstrapped symbols must not produce duplicate `# heading`
/// entries. Previously, `name_order` was pushed both by the bootstrap result
/// merge and the `by_name` insertion loop.
#[test]
fn test_grep_rg_bootstrap_no_duplicate_heading() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let file = dir.path().join(format!("dedup_test.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn dedup_target()\ndedup_target\n")?;

    // --symbol-limit 0 forces empty universe; all enrichment comes from bootstrap
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots --symbol-limit 0");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 2040,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "dedup_target" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep bootstrap dedup failed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text for dedup")?;

    let heading_count = text.matches("# dedup_target").count();
    assert_eq!(
        heading_count, 1,
        "Expected exactly 1 '# dedup_target' heading, got {heading_count}:\n{text}"
    );

    Ok(())
}

/// Regression: when hovering on a keyword token (`fn`) resolves to a different
/// name (`my_func`), the bootstrap must skip the keyword and use the hover
/// content from the actual symbol token. `--literal-keyword-hover` makes mockls
/// return the raw word on hover (like real LSPs), so hovering `fn` returns `fn`
/// while `prepareCallHierarchy` still resolves to the function name.
#[test]
fn test_grep_rg_bootstrap_keyword_hover_content() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let file = dir.path().join(format!("kw_hover.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn kw_hover_sym()\nkw_hover_sym\n")?;

    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        "--scan-roots --symbol-limit 0 --literal-keyword-hover",
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 2050,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "fn kw_hover_sym" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep bootstrap keyword hover content failed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text for keyword hover content")?;

    // Should still get enrichment — the keyword is skipped, the symbol token is used
    assert!(
        text.contains("## [Function]"),
        "Expected ## [Function] heading, got:\n{text}"
    );

    // Hover content must contain the symbol name, not the keyword
    assert!(
        text.contains("kw_hover_sym"),
        "Expected hover content with symbol name 'kw_hover_sym', got:\n{text}"
    );

    // The keyword text `fn` as a standalone hover block must not appear.
    // mockls hover format is ```\n{word}\n``` — check for keyword-only hover.
    assert!(
        !text.contains("```\nfn\n```"),
        "Hover content should be from the symbol, not the `fn` keyword:\n{text}"
    );

    Ok(())
}

/// Verify that LSP messages triggered by grep carry the MCP tool call's
/// `parent_id` in the database (misc 30: `parent_id` threading).
#[test]
fn test_grep_parent_id_threading() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let test_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn hello()\nhello\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "hello" }
        }
    }))?;
    let _response = bridge.recv()?;

    // Open the database and verify parent_id threading
    let db_path = PathBuf::from(root).join("catenary").join("catenary.db");
    let conn =
        rusqlite::Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .context("open test database")?;

    // Find the correlation ID of the tools/call MCP request.
    // MCP events now use in-process monotonic correlation IDs stored in
    // the `request_id` column (not the DB autoincrement `id`).
    let tool_call_corr_id: i64 = conn
        .query_row(
            "SELECT request_id FROM messages \
             WHERE type = 'mcp' AND method = 'tools/call' \
             AND request_id IS NOT NULL LIMIT 1",
            [],
            |row| row.get(0),
        )
        .context("find tools/call correlation ID")?;

    // LSP messages from the grep pipeline should carry this parent_id
    let lsp_with_parent: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE type = 'lsp' AND parent_id = ?1",
            [tool_call_corr_id],
            |row| row.get(0),
        )
        .context("count LSP messages with parent_id")?;

    assert!(
        lsp_with_parent > 0,
        "Expected LSP messages with parent_id={tool_call_corr_id} from grep, found 0"
    );

    Ok(())
}

/// Verify that LSP messages triggered by glob carry the MCP tool call's
/// `parent_id` in the database (misc 30: `parent_id` threading).
#[test]
fn test_glob_parent_id_threading() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let test_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn hello()\nhello\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "glob",
            "arguments": { "pattern": test_file.to_str().context("path")? }
        }
    }))?;
    let _response = bridge.recv()?;

    // Open the database and verify parent_id threading
    let db_path = PathBuf::from(root).join("catenary").join("catenary.db");
    let conn =
        rusqlite::Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .context("open test database")?;

    // Find the correlation ID of the tools/call MCP request.
    let tool_call_corr_id: i64 = conn
        .query_row(
            "SELECT request_id FROM messages \
             WHERE type = 'mcp' AND method = 'tools/call' \
             AND request_id IS NOT NULL LIMIT 1",
            [],
            |row| row.get(0),
        )
        .context("find tools/call correlation ID")?;

    // LSP messages from the glob pipeline should carry this parent_id
    let lsp_with_parent: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE type = 'lsp' AND parent_id = ?1",
            [tool_call_corr_id],
            |row| row.get(0),
        )
        .context("count LSP messages with parent_id")?;

    assert!(
        lsp_with_parent > 0,
        "Expected LSP messages with parent_id={tool_call_corr_id} from glob, found 0"
    );

    Ok(())
}

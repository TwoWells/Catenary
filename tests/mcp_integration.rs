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

/// Isolates a subprocess from the user's environment.
///
/// Sets `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, and `XDG_DATA_HOME` to the
/// given root so the process uses the test's tempdir instead of
/// `~/.config`, `~/.local/state`, or `~/.local/share`. Clears all
/// `CATENARY_*` env vars that could leak from the user's shell and
/// override test-specific settings.
///
/// All integration test subprocesses (bridge, `catenary install`, etc.)
/// must call this. Callers set `CATENARY_SERVERS`, `CATENARY_ROOTS`, or
/// `CATENARY_CONFIG` explicitly after this call.
fn isolate_env(cmd: &mut Command, root: &str) {
    cmd.env("XDG_CONFIG_HOME", root);
    cmd.env("XDG_STATE_HOME", root);
    cmd.env("XDG_DATA_HOME", root);
    cmd.env_remove("CATENARY_STATE_DIR");
    cmd.env_remove("CATENARY_DATA_DIR");
    cmd.env_remove("CATENARY_CONFIG");
    cmd.env_remove("CATENARY_SERVERS");
    cmd.env_remove("CATENARY_ROOTS");
}

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

        // Clear inherited env first, then set test-specific values
        if let Some(first_root) = roots.first() {
            isolate_env(&mut cmd, first_root);
        }
        cmd.env("CATENARY_SERVERS", lsp_commands.join(";"));
        let roots_val = std::env::join_paths(roots).unwrap_or_default();
        cmd.env("CATENARY_ROOTS", &roots_val);
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

    /// Spawn using a TOML config file instead of `CATENARY_SERVERS`.
    ///
    /// Required for multi-server-per-language tests where each server
    /// needs different flags.
    fn spawn_with_config(config_path: &std::path::Path, root: &str) -> Result<Self> {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        isolate_env(&mut cmd, root);
        cmd.env("CATENARY_CONFIG", config_path);
        cmd.env("CATENARY_ROOTS", root);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().context("Failed to spawn bridge")?;
        let stdin = child.stdin.take().context("Failed to get stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("Failed to get stdout")?);
        let stderr = child.stderr.take();

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr,
            state_home: Some(root.to_string()),
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
    #[allow(clippy::print_stderr, reason = "dump bridge logs on test failure")]
    fn drop(&mut self) {
        // If the test is panicking, dump bridge stderr for diagnostics.
        // This runs before we close stdin so the bridge is still alive
        // and may have buffered log output.
        if std::thread::panicking()
            && let Some(ref mut stderr) = self.stderr
        {
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf);
            if !buf.is_empty() {
                eprintln!("--- bridge stderr (test panicked) ---\n{buf}--- end bridge stderr ---");
            }
        }

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
        text_a.contains("alpha_func"),
        "Expected alpha_func in output, got: {text_a}"
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
        if text.contains("unique_root_b_func") && text.contains(&format!("funcs_b.{MOCK_LANG_A}")) {
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

/// Pre-installs the mock tree-sitter grammar into a test's isolated data dir.
///
/// Runs `catenary install <fixture_path>` with `XDG_DATA_HOME` pointing at
/// the test root so the grammar is written to the test's own directory,
/// not the user's `~/.local/share/catenary/grammars/`.
fn install_mock_grammar(state_home: &str) -> Result<()> {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test_assets")
        .join("mock_grammar");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("install")
        .arg(fixture.to_str().context("fixture path")?);
    isolate_env(&mut cmd, state_home);
    let output = cmd.output().context("failed to run catenary install")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("catenary install failed: {stderr}");
    }
    Ok(())
}

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
            text.contains("unique_root_b_func"),
            "Profile {name}: search in root B should find unique_root_b_func, got: {text}"
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
        text.contains("world"),
        "Root B search should find world, got: {text}"
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
        text_a.contains("greet"),
        "Lang A search should find symbol, got: {text_a}"
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
    // Should still find via ripgrep (no dependency on workspace/symbol)
    assert!(
        text.contains("greet"),
        "Search should find greet via ripgrep, got: {text}"
    );
    assert!(
        text.contains(&format!("test.{MOCK_LANG_A}")),
        "Search should find test file via ripgrep, got: {text}"
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
        text.contains("my_function"),
        "Search after warmup should find symbol, got: {text}"
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
        text.contains("greet"),
        "Search should find 'greet', got: {text}"
    );
    assert!(
        text.contains(&format!("greeter.{MOCK_LANG_A}")),
        "Search should find greeter file, got: {text}"
    );

    Ok(())
}

/// Verifies classified output for two symbols found via alternation.
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

    // Both symbols should appear in output
    assert!(
        text.contains("load_config"),
        "Expected load_config in output, got:\n{text}"
    );
    assert!(
        text.contains("save_config"),
        "Expected save_config in output, got:\n{text}"
    );

    // Both files should appear
    assert!(
        text.contains(&format!("mod_a.{MOCK_LANG_A}")),
        "Expected mod_a file, got:\n{text}"
    );
    assert!(
        text.contains(&format!("mod_b.{MOCK_LANG_A}")),
        "Expected mod_b file, got:\n{text}"
    );

    Ok(())
}

/// Verifies that grep finds symbols via ripgrep + prepareRename (no grammar).
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

    assert!(
        text.contains("resolve_me"),
        "Expected resolve_me in output, got:\n{text}"
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
        text.contains("alpha_func"),
        "Expected alpha_func symbol, got: {text}"
    );
    assert!(
        text.contains("beta_func"),
        "Expected beta_func symbol, got: {text}"
    );

    Ok(())
}

/// Verifies that broad search with many symbols produces flat output.
/// (Output budget and tier selection are in ticket 06b.)
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

    // All 11 symbols should appear in the flat output
    assert!(
        text.contains("zz_broad_one"),
        "Expected zz_broad_one in output, got: {text}"
    );
    assert!(
        text.contains("zz_broad_eleven"),
        "Expected zz_broad_eleven in output, got: {text}"
    );
    // Each should have file:line references
    assert!(
        text.contains(&format!("many.{MOCK_LANG_A}")),
        "Expected file reference, got: {text}"
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

    // Both tokens should appear in output
    assert!(
        text.contains("alpha_token"),
        "Expected alpha_token in output, got:\n{text}"
    );
    assert!(
        text.contains("beta_token"),
        "Expected beta_token in output, got:\n{text}"
    );

    // Both files should appear
    assert!(
        text.contains(&format!("notes.{MOCK_LANG_A}")),
        "Expected notes file, got:\n{text}"
    );
    assert!(
        text.contains(&format!("readme.{MOCK_LANG_A}")),
        "Expected readme file, got:\n{text}"
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

    // Both symbols should appear in output
    assert!(
        text.contains("compute"),
        "Expected compute in output, got:\n{text}"
    );
    assert!(
        text.contains("render"),
        "Expected render in output, got:\n{text}"
    );

    // Both files should appear
    assert!(
        text.contains(&format!("engine.{MOCK_LANG_A}")),
        "Expected engine file, got:\n{text}"
    );
    assert!(
        text.contains(&format!("display.{MOCK_LANG_A}")),
        "Expected display file, got:\n{text}"
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

    // Both files should appear with the symbol
    assert!(
        text.contains("process"),
        "Expected process in output, got:\n{text}"
    );
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

    // Symbol should be found via ripgrep + prepareRename
    assert!(
        text.contains("resolve_fallback"),
        "Expected resolve_fallback in output, got:\n{text}"
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

    // Symbol should appear with both files
    assert!(
        text.contains("cross_server_fn"),
        "Expected cross_server_fn in output, got:\n{text}"
    );
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

    // Enrichment runs (07a) but tier 1 rendering is 07b — verify symbol found
    assert!(
        text.contains("callee_fn"),
        "Expected callee_fn in output, got:\n{text}"
    );
    assert!(
        text.contains(&format!("calls.{MOCK_LANG_A}")),
        "Expected file name in output, got:\n{text}"
    );

    Ok(())
}

/// Verifies that enrichment for structs runs without error.
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

    // Enrichment runs (07a) but tier 1 rendering is 07b — verify symbol found
    assert!(
        text.contains("MyStruct"),
        "Expected MyStruct in output, got:\n{text}"
    );

    Ok(())
}

/// Verifies that enrichment for types with subtypes runs without error.
#[test]
fn test_grep_enrichment_subtypes() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let test_file = dir.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(
        &test_file,
        "interface Animal\nstruct Dog extends Animal\nclass Cat implements Animal\n",
    )?;

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

    // Enrichment runs (07a) but tier 1 rendering is 07b — verify symbol found
    assert!(
        text.contains("Animal"),
        "Expected Animal in output, got:\n{text}"
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
        if let Some(t) = result["content"][0]["text"].as_str()
            && t.contains("zz_widget_method")
        {
            text = t.to_string();
            break;
        }
    }

    // Flush stderr so we can see the output in test logs
    let _ = writeln!(
        std::io::stderr(),
        "\n=== ra_symbol_universe output ===\n{text}\n=== end ==="
    );

    assert!(
        text.contains("zz_widget_method"),
        "Expected 'zz_widget_method' in output, got:\n{text}"
    );

    Ok(())
}

// ─── SEARCHv2 grep pipeline tests (ticket 06a) ─────────────────────────

/// Pattern matching a known symbol — no grammar installed, so the
/// no-grammar path (prepareRename) identifies symbols.
#[test]
fn test_grep_basic_hits() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("greet.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn say_hello()\nsay_hello\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3000,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "say_hello" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Should find the symbol with file and line reference
    assert!(
        text.contains("say_hello"),
        "Expected say_hello in output, got:\n{text}"
    );
    assert!(
        text.contains(&format!("greet.{MOCK_LANG_A}")),
        "Expected filename in output, got:\n{text}"
    );

    Ok(())
}

/// Grep with `glob` scoping — only matching files appear.
#[test]
fn test_grep_glob_scoping() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let src_dir = dir.path().join("src");
    std::fs::create_dir(&src_dir)?;
    let file_a = src_dir.join(format!("a.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn scope_target()\nscope_target\n")?;
    let file_b = dir.path().join(format!("b.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn scope_target()\nscope_target\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3001,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": {
                "pattern": "scope_target",
                "glob": "src/**"
            }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    assert!(
        text.contains(&format!("a.{MOCK_LANG_A}")),
        "Expected src/a file in output, got:\n{text}"
    );
    assert!(
        !text.contains(&format!("b.{MOCK_LANG_A}")),
        "Expected b file excluded from glob scope, got:\n{text}"
    );

    Ok(())
}

/// Grep with `exclude` — test files excluded from matches.
#[test]
fn test_grep_exclude() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_a = dir.path().join(format!("main.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn excl_func()\nexcl_func\n")?;
    let file_b = dir.path().join(format!("test_main.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn excl_func()\nexcl_func\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3002,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": {
                "pattern": "excl_func",
                "exclude": "**/test_*"
            }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    assert!(
        text.contains(&format!("main.{MOCK_LANG_A}")),
        "Expected main file in output, got:\n{text}"
    );
    assert!(
        !text.contains(&format!("test_main.{MOCK_LANG_A}")),
        "Expected test file excluded, got:\n{text}"
    );

    Ok(())
}

/// `foo|bar` pattern produces two independent result sections.
#[test]
fn test_grep_alternation_split() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_a = dir.path().join(format!("alt_a.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn alt_alpha()\nalt_alpha\n")?;
    let file_b = dir.path().join(format!("alt_b.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn alt_beta()\nalt_beta\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3004,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "alt_alpha|alt_beta" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    assert!(
        text.contains("alt_alpha"),
        "Expected alt_alpha in output, got:\n{text}"
    );
    assert!(
        text.contains("alt_beta"),
        "Expected alt_beta in output, got:\n{text}"
    );

    Ok(())
}

/// `(foo|bar)_baz` pattern is a single result section (not split).
#[test]
fn test_grep_alternation_nested() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("nested.{MOCK_LANG_A}"));
    std::fs::write(
        &file,
        "fn alpha_baz()\nalpha_baz\nfn beta_baz()\nbeta_baz\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3005,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "(alpha|beta)_baz" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Both matches should appear — the nested alternation is one arm
    assert!(
        text.contains("alpha_baz") && text.contains("beta_baz"),
        "Expected both alpha_baz and beta_baz in single section, got:\n{text}"
    );

    Ok(())
}

/// No-grammar file, pattern matches only keywords. Keywords filtered out
/// via `prepareRename` returning null should not appear in output.
/// Searches for the keyword `struct` which mockls recognizes and returns
/// null for via prepareRename.
#[test]
fn test_grep_prepare_rename_keyword() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("kw.{MOCK_LANG_A}"));
    // Only the keyword `struct` matches — mockls returns null for keywords
    std::fs::write(&file, "struct MyType\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Search for the keyword `struct` (not the symbol name)
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3006,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "^struct$" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // The keyword `struct` is filtered out by prepareRename returning null.
    // Only the keyword itself matched (not the symbol name), so output is empty.
    assert_eq!(
        text, "No results found",
        "Expected 'No results found' when only keywords match, got:\n{text}"
    );

    Ok(())
}

/// Tree-sitter kind labels use `<Kind>` angle brackets.
/// Requires the mock grammar to be installed so tree-sitter can classify symbols.
#[test]
fn test_grep_kind_brackets() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // Install mock grammar into the test's state dir
    install_mock_grammar(root)?;

    // Create a .mock file — the mock grammar parses `fn name` and `struct name`
    let file = dir.path().join("kinds.mock");
    std::fs::write(&file, "fn my_func\nstruct MyStruct\n")?;

    // No LSP needed for tree-sitter classification — use mockls as a no-op server
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3010,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "my_func|MyStruct" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Tree-sitter kind labels should use <Kind> angle brackets
    assert!(
        text.contains("<Function>"),
        "Expected <Function> kind label, got:\n{text}"
    );
    assert!(
        text.contains("<Struct>"),
        "Expected <Struct> kind label, got:\n{text}"
    );
    // Must NOT use [Kind] square brackets (old format)
    assert!(
        !text.contains("[Function]") && !text.contains("[Struct]"),
        "Expected angle brackets <Kind>, not square brackets [Kind], got:\n{text}"
    );

    Ok(())
}

/// Reference hit at a non-definition line reports enclosing tree-sitter structure.
/// Uses the mock grammar's brace-delimited block syntax: `fn outer { target }`.
#[test]
fn test_grep_reference_enclosing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    install_mock_grammar(root)?;

    // fn outer spans lines 0-2, "target" on line 1 is enclosed by it
    let file = dir.path().join("enclosing.mock");
    std::fs::write(&file, "fn outer {\ntarget\n}\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3011,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "target" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Tier 2 format: `:line <Kind> name:span`
    assert!(
        text.contains("<Function>") && text.contains("outer"),
        "Expected enclosing <Function> outer in output, got:\n{text}"
    );
    // Span should show the enclosing function's range
    assert!(
        text.contains(":1-3"),
        "Expected enclosing span :1-3, got:\n{text}"
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

// ─── 06b: Tier selection and rendering ──────────────────────────────────

/// Tier 2 structure heatmap: names grouped with enclosing structures and spans.
#[test]
fn test_grep_tier2_structure_heatmap() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    install_mock_grammar(root)?;

    // Create multiple .mock files with definitions and references
    let tests_dir = dir.path().join("tests");
    std::fs::create_dir(&tests_dir)?;
    let file_a = tests_dir.join("alpha.mock");
    std::fs::write(&file_a, "fn test_alpha {\ntest_alpha\n}\n")?;
    let file_b = tests_dir.join("beta.mock");
    std::fs::write(&file_b, "fn test_beta {\ntest_alpha\n}\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 4000,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "test_alpha" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Name group at column 0 (no leading whitespace)
    let first_line = text.lines().next().unwrap_or("");
    assert!(
        !first_line.starts_with('\t') && !first_line.starts_with(' '),
        "Name group should be at column 0, got:\n{text}"
    );

    // Enclosing structures with spans
    assert!(
        text.contains("<Function>"),
        "Expected <Function> kind label, got:\n{text}"
    );

    // Directory grouping
    assert!(
        text.contains("tests/"),
        "Expected tests/ directory grouping, got:\n{text}"
    );

    Ok(())
}

/// Tier 2 for no-grammar file: bare hit lines without enclosing structures.
#[test]
fn test_grep_tier2_no_grammar() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("data.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn say_hello()\nsay_hello\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 4001,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "say_hello" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // No-grammar files still produce output (via prepareRename)
    assert!(
        text.contains("say_hello"),
        "Expected symbol in output, got:\n{text}"
    );
    // Bare hit lines use `:line` format
    assert!(
        text.contains(':'),
        "Expected line numbers in output, got:\n{text}"
    );

    Ok(())
}

/// Narrow pattern fits tier 2: assert tier 2 output with name groups.
#[test]
fn test_grep_tier_promotion() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    install_mock_grammar(root)?;

    let file = dir.path().join("narrow.mock");
    std::fs::write(&file, "fn unique_symbol_xyz\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 4002,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "unique_symbol_xyz" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Tier 2 format: name at column 0, indented file tree
    assert!(
        text.contains("unique_symbol_xyz"),
        "Expected name in output, got:\n{text}"
    );
    assert!(
        text.contains("<Function>"),
        "Expected <Function> kind, got:\n{text}"
    );

    // Not tier 3 bucketed (no wildcard patterns)
    assert!(
        !text.contains("_*"),
        "Expected tier 2, not tier 3 bucketed, got:\n{text}"
    );

    Ok(())
}

/// Single-line structure: `:line <Kind> name:line` (no range).
#[test]
fn test_grep_single_line_structure() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    install_mock_grammar(root)?;

    // Single-line definition (no brace block)
    let file = dir.path().join("single.mock");
    std::fs::write(&file, "fn one_liner\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 4003,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "one_liner" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    assert!(
        text.contains("<Function>") && text.contains("one_liner"),
        "Expected function definition, got:\n{text}"
    );
    // Single-line: `:1` not `:1-1`
    assert!(
        text.contains(":1") && !text.contains(":1-1"),
        "Single-line structure should show :line not :start-end, got:\n{text}"
    );

    Ok(())
}

/// No blank line separators between name groups.
#[test]
fn test_grep_no_blank_lines() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    install_mock_grammar(root)?;

    let file = dir.path().join("multi.mock");
    std::fs::write(&file, "fn alpha_one\nfn beta_two\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 4004,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "alpha_one|beta_two" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Each alternation arm produces its own output section
    assert!(
        text.contains("alpha_one"),
        "Expected alpha_one, got:\n{text}"
    );
    assert!(text.contains("beta_two"), "Expected beta_two, got:\n{text}");

    // No blank lines within a single arm's output
    for arm_text in [&text] {
        let lines: Vec<&str> = arm_text.lines().collect();
        for window in lines.windows(2) {
            assert!(
                !(window[0].is_empty() && window[1].is_empty()),
                "Found consecutive blank lines in output:\n{text}"
            );
        }
    }

    Ok(())
}

/// Multi-server priority chain for `prepareRename`: first server errors,
/// second server succeeds. The symbol should still appear in output.
///
/// Uses two mockls servers for the same language. Server A has
/// `--fail-on textDocument/prepareRename`; server B works normally.
/// No grammar installed, so the no-grammar path exercises
/// `prepare_rename_check` priority chain fallthrough.
#[test]
fn test_grep_prepare_rename_priority_chain() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    let file = dir.path().join(format!("chain.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn chain_symbol\nchain_symbol\n")?;

    // Config with two servers: first fails on prepareRename, second works
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[server.mockls-fail]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"{MOCK_LANG_A}\", \"--scan-roots\", \"--fail-on\", \"textDocument/prepareRename\"]\n\n\
             [server.mockls-ok]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"{MOCK_LANG_A}\", \"--scan-roots\"]\n\n\
             [language.{MOCK_LANG_A}]\n\
             servers = [\"mockls-fail\", \"mockls-ok\"]\n"
        ),
    )?;

    let mut bridge = BridgeProcess::spawn_with_config(&config_path, root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 4100,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "chain_symbol" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Server A errors on prepareRename, server B succeeds.
    // The symbol should appear despite the first server failing.
    assert!(
        text.contains("chain_symbol"),
        "Expected chain_symbol in output (priority chain fallthrough), got:\n{text}"
    );

    Ok(())
}

// ─── SEARCHv2 enrichment tests (ticket 07a) ───────────────────────────

/// Enrich a function: `outgoing_calls` and `ref_lines` are populated.
/// Uses the no-grammar path (mockls, no tree-sitter grammar installed).
/// Enrichment runs via the pipeline; output is still tier 2.
#[test]
fn test_enrich_ungated_function() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // callee_fn defined on L0, caller_fn defined on L1, caller_fn calls callee_fn on L2
    let file = dir.path().join(format!("func.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn callee_fn()\nfn caller_fn()\n  callee_fn\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 5100,
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
        "enrich_ungated_function should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Tool completes successfully with enrichment running (tier 2 output)
    assert!(
        text.contains("callee_fn"),
        "Expected callee_fn in output, got:\n{text}"
    );

    Ok(())
}

/// Enrich a type: implementations, supertypes, subtypes are populated.
/// Uses the no-grammar path.
#[test]
fn test_enrich_ungated_type() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let file = dir.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(
        &file,
        "interface Vehicle\nstruct Car extends Vehicle\nclass Truck implements Vehicle\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 5110,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "Vehicle" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "enrich_ungated_type should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    assert!(
        text.contains("Vehicle"),
        "Expected Vehicle in output, got:\n{text}"
    );

    Ok(())
}

/// `from_ts=true` path: tree-sitter-identified symbol skips `prepareRename`.
/// Installs mock grammar so hits are `HitClass::Symbol` (`from_ts=true`).
#[test]
fn test_enrich_from_ts_true() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    install_mock_grammar(root)?;

    // .mock file with a function definition
    let file = dir.path().join("ts_true.mock");
    std::fs::write(&file, "fn my_symbol\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 5120,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "my_symbol" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "enrich from_ts=true should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Tree-sitter identified the symbol — enrichment runs without prepareRename
    assert!(
        text.contains("my_symbol"),
        "Expected my_symbol in output, got:\n{text}"
    );
    assert!(
        text.contains("<Function>"),
        "Expected tree-sitter kind label, got:\n{text}"
    );

    Ok(())
}

/// `from_ts=false` on a symbol: prepareRename returns range, enrichment proceeds.
/// No grammar installed, so the no-grammar path exercises prepareRename.
#[test]
fn test_enrich_from_ts_false_symbol() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let file = dir.path().join(format!("sym.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn enrichable_sym\nenrichable_sym\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 5130,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "enrichable_sym" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "enrich from_ts=false symbol should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // prepareRename confirmed symbol, enrichment ran
    assert!(
        text.contains("enrichable_sym"),
        "Expected enrichable_sym in output, got:\n{text}"
    );

    Ok(())
}

/// `from_ts=false` on a keyword: `prepareRename` returns null, enrichment skipped.
/// Keywords are dropped entirely (no output for keyword-only matches).
/// Uses `fn` which is in mockls's keyword list so `prepareRename` returns null.
#[test]
fn test_enrich_from_ts_false_keyword() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // `fn` is a keyword — mockls returns null for prepareRename on keywords.
    // The file defines `fn my_symbol` but the grep pattern is `^fn$`, matching
    // only the keyword itself (not the symbol name).
    let file = dir.path().join(format!("kw.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn my_symbol\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Pattern `^fn$` matches the keyword `fn` at column 0 but not `my_symbol`.
    // Since `fn` is in mockls's keyword list, prepareRename returns null.
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 5140,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "^fn " }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // The keyword `fn` is filtered by prepareRename returning null.
    assert_eq!(
        text, "No results found",
        "Expected 'No results found' for keyword-only match, got:\n{text}"
    );

    Ok(())
}

/// Deprecated subtype: TypeEdge.deprecated is set from tags.
/// Enrichment runs for the interface; mockls returns deprecated subtypes
/// when the declaration line contains @deprecated.
#[test]
fn test_enrich_deprecated_type_edge() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let file = dir.path().join(format!("depr.{MOCK_LANG_A}"));
    std::fs::write(
        &file,
        "interface Shape\nstruct OldSquare extends Shape @deprecated\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 5150,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "Shape" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "enrich deprecated type edge should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Tool completes with deprecated subtypes collected (rendered in 07b)
    assert!(
        text.contains("Shape"),
        "Expected Shape in output, got:\n{text}"
    );

    Ok(())
}

/// Function with callees: `outgoing_calls` has correct names, kinds, files, lines.
/// Uses mockls which implements outgoing calls by scanning for known function
/// names called within the body.
#[test]
fn test_enrich_outgoing_calls() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // helper_a and helper_b defined, then main_fn calls them
    let file = dir.path().join(format!("out.{MOCK_LANG_A}"));
    std::fs::write(
        &file,
        "fn helper_a()\nfn helper_b()\nfn main_fn()\n  helper_a\n  helper_b\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 5160,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "main_fn" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "enrich outgoing calls should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Enrichment ran — outgoing calls collected (rendered in 07b)
    assert!(
        text.contains("main_fn"),
        "Expected main_fn in output, got:\n{text}"
    );

    Ok(())
}

// SPDX-License-Identifier: GPL-3.0-or-later
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

use std::io::{BufRead, BufReader, Write};
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
}

impl BridgeProcess {
    fn spawn(lsp_commands: &[&str], root: &str) -> Result<Self> {
        Self::spawn_multi_root(lsp_commands, &[root])
    }

    fn spawn_multi_root(lsp_commands: &[&str], roots: &[&str]) -> Result<Self> {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));

        for lsp in lsp_commands {
            cmd.arg("--lsp").arg(lsp);
        }

        for root in roots {
            cmd.arg("--root").arg(root);
        }

        // Isolate from user-level config
        if let Some(first_root) = roots.first() {
            cmd.env("XDG_CONFIG_HOME", first_root);
        }
        cmd.stdin(Stdio::piped())
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
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], "/tmp")?;

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
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], "/tmp")?;
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

    // Check all expected tools are present (5 after status removal)
    let expected_tools = [
        "search",
        "document_symbols",
        "diagnostics",
        "codebase_map",
        "list_directory",
    ];

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
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], "/tmp")?;
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
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], "/tmp")?;
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
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], "/tmp")?;

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

    // Run catenary list and check output
    let output = Command::new(env!("CARGO_BIN_EXE_catenary"))
        .arg("list")
        .output()
        .context("Failed to run catenary list")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Note: The client name may be truncated in the list output, so we only check for TestClient
    assert!(
        stdout.contains("TestClient"),
        "Expected client info 'TestClient' in catenary list output, got:\n{stdout}"
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
            "name": "search",
            "arguments": { "queries": ["alpha_func"] }
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
        text_a.contains("## Symbols"),
        "Expected Symbols section for alpha_func, got: {text_a}"
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
            "name": "search",
            "arguments": { "queries": ["beta_func"] }
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
fn test_multi_root_document_symbols() -> Result<()> {
    // Create two roots with different symbols
    let dir_a = tempfile::tempdir().context("Failed to create temp dir A")?;
    let dir_b = tempfile::tempdir().context("Failed to create temp dir B")?;

    let script_a = dir_a.path().join(format!("syms_a.{MOCK_LANG_A}"));
    std::fs::write(&script_a, "function sym_alpha()\nfunction sym_beta()\n")?;

    let script_b = dir_b.path().join(format!("syms_b.{MOCK_LANG_A}"));
    std::fs::write(&script_b, "function sym_gamma()\nfunction sym_delta()\n")?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp], &[root_a, root_b])?;
    bridge.initialize()?;

    // Get symbols from root A file
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 720,
        "method": "tools/call",
        "params": {
            "name": "document_symbols",
            "arguments": {
                "file": script_a.to_str().context("Invalid script A path")?
            }
        }
    }))?;

    let response_a = bridge.recv()?;
    let result_a = &response_a["result"];
    assert!(
        result_a["isError"].is_null() || result_a["isError"] == false,
        "Document symbols from root A failed: {response_a:?}"
    );
    let text_a = result_a["content"][0]["text"]
        .as_str()
        .context("Missing text for symbols A")?;
    assert!(
        text_a.contains("sym_alpha"),
        "Should contain sym_alpha, got: {text_a}"
    );
    assert!(
        text_a.contains("sym_beta"),
        "Should contain sym_beta, got: {text_a}"
    );

    // Get symbols from root B file
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 721,
        "method": "tools/call",
        "params": {
            "name": "document_symbols",
            "arguments": {
                "file": script_b.to_str().context("Invalid script B path")?
            }
        }
    }))?;

    let response_b = bridge.recv()?;
    let result_b = &response_b["result"];
    assert!(
        result_b["isError"].is_null() || result_b["isError"] == false,
        "Document symbols from root B failed: {response_b:?}"
    );
    let text_b = result_b["content"][0]["text"]
        .as_str()
        .context("Missing text for symbols B")?;
    assert!(
        text_b.contains("sym_gamma"),
        "Should contain sym_gamma, got: {text_b}"
    );
    assert!(
        text_b.contains("sym_delta"),
        "Should contain sym_delta, got: {text_b}"
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
            "name": "search",
            "arguments": { "queries": ["unique_root_a_func"] }
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
                "name": "search",
                "arguments": { "queries": ["unique_root_b_func"] }
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
        if text.contains("## Symbols") && text.contains(&format!("funcs_b.{MOCK_LANG_A}")) {
            success = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    assert!(
        success,
        "Search in root B should find ## Symbols with funcs_b.mock after server restart. Last output: {last_text}"
    );

    Ok(())
}

// ─── roots/list tests ───────────────────────────────────────────────────

#[test]
fn test_roots_list_after_initialize() -> Result<()> {
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], "/tmp")?;

    // Initialize with roots capability — this validates the full round-trip:
    // initialize → notifications/initialized → server sends roots/list →
    // client responds → server applies roots
    bridge.initialize_with_roots(&["/tmp"])?;

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
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], "/tmp")?;

    // Initialize with roots capability
    bridge.initialize_with_roots(&["/tmp"])?;

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
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], "/tmp")?;

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

/// Build an `--lsp` argument for `BridgeProcess::spawn` using mockls.
fn mockls_lsp_arg(lang: &str, flags: &str) -> String {
    let bin = env!("CARGO_BIN_EXE_mockls");
    if flags.is_empty() {
        format!("{lang}:{bin} {lang}")
    } else {
        format!("{lang}:{bin} {lang} {flags}")
    }
}

#[test]
fn test_mockls_diagnostics_across_profiles() -> Result<()> {
    let profiles: &[(&str, &str)] = &[
        ("version", "--publish-version"),
        (
            "on-save",
            "--diagnostics-on-save --publish-version --advertise-save",
        ),
        ("suppressed", "--no-diagnostics"),
        ("degraded", ""),
    ];

    for (name, flags) in profiles {
        let dir = tempfile::tempdir().context("Failed to create temp dir")?;
        let test_file_path = dir.path().join(format!("mockls_diag_test.{MOCK_LANG_A}"));
        std::fs::write(&test_file_path, "echo hello\n")?;
        let test_file = test_file_path.to_str().context("Invalid test file path")?;

        let lsp = mockls_lsp_arg(MOCK_LANG_A, flags);
        let root = dir.path().to_str().context("root path")?;
        let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
        bridge.initialize()?;

        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "diagnostics",
                "arguments": {
                    "file": test_file
                }
            }
        }))?;

        let response = bridge.recv()?;
        let result = &response["result"];

        // All profiles should return without hanging
        let content = result["content"]
            .as_array()
            .context(format!("Profile {name}: missing content array"))?;

        let text = content[0]["text"]
            .as_str()
            .context(format!("Profile {name}: missing text"))?;

        if *name == "suppressed" || *name == "degraded" {
            assert!(
                text.contains("No diagnostics") || text.contains("0 diagnostics"),
                "Profile {name}: expected no diagnostics, got: {text}"
            );
        } else {
            assert!(
                text.contains("mock diagnostic") || text.contains("mockls"),
                "Profile {name}: expected mock diagnostics, got: {text}"
            );
        }
    }
    Ok(())
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
                "name": "search",
                "arguments": { "queries": ["unique_root_a_func"] }
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
                "name": "search",
                "arguments": { "queries": ["unique_root_b_func"] }
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
            text.contains("## Symbols"),
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
            "name": "search",
            "arguments": { "queries": ["hello"] }
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
            "name": "search",
            "arguments": { "queries": ["world"] }
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
        text.contains("## Symbols"),
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
            "name": "search",
            "arguments": { "queries": ["greet"] }
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
            "name": "search",
            "arguments": { "queries": ["package"] }
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
        text_a.contains("## Symbols"),
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

    // Call diagnostics — this triggers didOpen + didSave internally
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "diagnostics",
            "arguments": { "file": test_file.to_str().context("file path")? }
        }
    }))?;
    let _ = bridge.recv()?;

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

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "diagnostics",
            "arguments": { "file": test_file.to_str().context("file path")? }
        }
    }))?;
    let _ = bridge.recv()?;

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
/// Search should still return ripgrep file matches.
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
            "name": "search",
            "arguments": { "queries": ["greet"] }
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
    // Should NOT have a Symbols section (LSP failed)
    assert!(
        !text.contains("## Symbols"),
        "Symbols section should be absent when workspace/symbol fails, got: {text}"
    );

    Ok(())
}

/// Verifies that `wait_ready` detects failure when the server burns CPU
/// without progress tokens after a workspace folder change (Gap 3).
///
/// mockls `--cpu-on-workspace-change 15000` burns 15s of CPU (1500 ticks)
/// on `workspace/didChangeWorkspaceFolders`. Catenary sets the server to
/// Busy, but `progress_active()` returns false (no actual `$/progress`
/// tokens in the tracker). The failure threshold (1000 ticks) drains and
/// `wait_ready` returns false, producing an error.
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

    // Send a search request — wait_ready should detect failure
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": { "queries": ["hello"] }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];

    // Dead servers are non-fatal — search degrades gracefully with ripgrep results
    assert!(
        result.get("isError").is_none() || result["isError"] == false,
        "Dead server should degrade gracefully, not error. Got: {response:?}"
    );

    // Offline notification should be prepended
    let content = result["content"]
        .as_array()
        .context("Missing content array")?;
    assert!(
        content.len() >= 2,
        "Expected notification + search results. Got: {content:?}"
    );
    let notification = content[0]["text"]
        .as_str()
        .context("Missing notification text")?;
    assert!(
        notification.contains("server offline"),
        "Expected offline notification. Got: {notification}"
    );

    Ok(())
}

/// Verifies warmup observation: `is_ready()` waits for the server to
/// become Sleeping before declaring it ready (Gap 6).
///
/// mockls `--cpu-on-initialized 3000` burns 3s of CPU on `initialized`.
/// During warmup (<3s from spawn), the server is Running, so `is_ready()`
/// returns false. After the burn completes, the server goes Sleeping,
/// `is_ready()` returns true, and the search request succeeds.
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
            "name": "search",
            "arguments": { "queries": ["my_function"] }
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
        text.contains("## Symbols"),
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
            "name": "search",
            "arguments": { "queries": ["greet"] }
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
        text.contains("## Symbols"),
        "Search with --scan-roots should produce ## Symbols section, got: {text}"
    );
    assert!(
        text.contains("greet"),
        "Symbols section should contain 'greet', got: {text}"
    );

    Ok(())
}

/// Verifies that type definition enrichment appears in search output.
/// mockls resolves `let result: MyType` to the `struct MyType` declaration
/// in the other file, so the `Type:` field should appear in the enrichment.
#[test]
fn test_search_type_definition_enrichment() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // Create two files: one with a type definition, another with a
    // variable annotated with that type. typeDefinition on the variable
    // resolves to the struct declaration in the other file.
    let def_file = dir.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(&def_file, "struct MyType\n")?;

    let use_file = dir.path().join(format!("usage.{MOCK_LANG_A}"));
    std::fs::write(&use_file, "let result: MyType\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Search for 'result' — should find the variable in usage.mock
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": { "queries": ["result"] }
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
        text.contains("## Symbols"),
        "Expected Symbols section, got: {text}"
    );
    // The type definition enrichment should show Type: for the variable
    // symbol, pointing to wherever mockls resolves the type definition.
    assert!(
        text.contains("Type:"),
        "Expected Type: enrichment for variable symbol, got: {text}"
    );
    assert!(
        text.contains(&format!("types.{MOCK_LANG_A}")),
        "Type: should reference types.mock, got: {text}"
    );

    Ok(())
}

/// Verifies that type definition enrichment points to the correct cross-file
/// type declaration. File A has `struct Counter`, file B has `let count: Counter`.
/// Searching for `count` should show `Type:` pointing to file A.
#[test]
fn test_search_type_definition_cross_file() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let type_file = dir.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(&type_file, "struct Counter\n")?;

    let use_file = dir.path().join(format!("usage.{MOCK_LANG_A}"));
    std::fs::write(&use_file, "let count: Counter\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": { "queries": ["count"] }
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
        text.contains("## Symbols"),
        "Expected Symbols section, got: {text}"
    );
    assert!(
        text.contains("Type:"),
        "Expected Type: enrichment for variable symbol, got: {text}"
    );
    assert!(
        text.contains(&format!("types.{MOCK_LANG_A}")),
        "Type: should point to types.mock, got: {text}"
    );

    Ok(())
}

/// Verifies that call hierarchy enrichment appears for function symbols.
/// File has `fn caller()` body calling `fn callee()`. Search for `callee`
/// should show `Called by:` with `caller`.
#[test]
fn test_search_call_hierarchy_enrichment() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let test_file = dir.path().join(format!("calls.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn callee()\nfn caller()\n  callee\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": { "queries": ["callee"] }
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
        text.contains("## Symbols"),
        "Expected Symbols section, got: {text}"
    );
    assert!(
        text.contains("Called by:"),
        "Expected Called by: enrichment for function symbol, got: {text}"
    );
    assert!(
        text.contains("caller"),
        "Called by: should mention caller, got: {text}"
    );

    Ok(())
}

/// Verifies that struct symbols get implementation enrichment.
/// File has `struct Foo` and references. Search for `Foo` should show
/// `Implementations:`.
#[test]
fn test_search_struct_implementations() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let test_file = dir.path().join(format!("structs.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "struct Foo\nlet x: Foo\nFoo\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": { "queries": ["Foo"] }
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
        text.contains("## Symbols"),
        "Expected Symbols section, got: {text}"
    );
    assert!(
        text.contains("Implementations:"),
        "Expected Implementations: enrichment for struct symbol, got: {text}"
    );
    assert!(
        text.contains(&format!("structs.{MOCK_LANG_A}")),
        "Implementations should reference structs.mock, got: {text}"
    );

    Ok(())
}

/// Verifies that interface symbols get subtype enrichment.
/// File has `interface Animal` and a `struct Dog`. Search for `Animal`
/// should show `Subtypes:`.
#[test]
fn test_search_interface_subtypes() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let test_file = dir.path().join(format!("iface.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "interface Animal\nstruct Dog\nAnimal\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": { "queries": ["Animal"] }
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
        text.contains("## Symbols"),
        "Expected Symbols section, got: {text}"
    );
    assert!(
        text.contains("Subtypes:"),
        "Expected Subtypes: enrichment for interface symbol, got: {text}"
    );
    assert!(
        text.contains("Dog"),
        "Subtypes should contain Dog, got: {text}"
    );

    Ok(())
}

/// Verifies that when multiple symbols share a name, references are grouped
/// under their respective definitions via import-based scope resolution.
/// File A has `fn load_config()`, file B has `fn load_config()`,
/// file C imports from A, file D imports from B.
#[test]
#[allow(
    clippy::too_many_lines,
    clippy::similar_names,
    reason = "disambiguation test requires many assertions and group_1/group_2 variables"
)]
fn test_search_disambiguated_references() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let file_a = dir.path().join(format!("mod_a.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn load_config()\n")?;

    let file_b = dir.path().join(format!("mod_b.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn load_config()\n")?;

    let file_c = dir.path().join(format!("caller_c.{MOCK_LANG_A}"));
    std::fs::write(
        &file_c,
        "from mod_a import load_config\nfn use_a()\n  load_config\n",
    )?;

    let file_d = dir.path().join(format!("caller_d.{MOCK_LANG_A}"));
    std::fs::write(
        &file_d,
        "from mod_b import load_config\nfn use_b()\n  load_config\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": { "queries": ["load_config"] }
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
        text.contains("## Symbols"),
        "Expected Symbols section, got: {text}"
    );
    assert!(
        text.contains(&format!("mod_a.{MOCK_LANG_A}")),
        "Expected mod_a.mock in output, got: {text}"
    );
    assert!(
        text.contains(&format!("mod_b.{MOCK_LANG_A}")),
        "Expected mod_b.mock in output, got: {text}"
    );
    assert!(
        text.contains("## References"),
        "Expected References section for multiple symbols, got: {text}"
    );
    // Both disambiguation groups must exist
    assert!(
        text.contains("load_config (1):"),
        "Expected disambiguation group (1), got:\n{text}"
    );
    assert!(
        text.contains("load_config (2):"),
        "Expected disambiguation group (2), got:\n{text}"
    );

    // Extract the References section
    let refs_section = text
        .split("## References")
        .nth(1)
        .and_then(|s| s.split("## File matches").next())
        .unwrap_or("");

    // Determine which group number each definition got
    let mod_a_in_group_1 = refs_section.contains(&format!("mod_a.{MOCK_LANG_A} load_config (1):"));
    let mod_a_in_group_2 = refs_section.contains(&format!("mod_a.{MOCK_LANG_A} load_config (2):"));
    assert!(
        mod_a_in_group_1 || mod_a_in_group_2,
        "mod_a should appear in a group header, got:\n{refs_section}"
    );

    let mod_b_in_group_1 = refs_section.contains(&format!("mod_b.{MOCK_LANG_A} load_config (1):"));
    let mod_b_in_group_2 = refs_section.contains(&format!("mod_b.{MOCK_LANG_A} load_config (2):"));
    assert!(
        mod_b_in_group_1 || mod_b_in_group_2,
        "mod_b should appear in a group header, got:\n{refs_section}"
    );

    // They must be in different groups
    assert!(
        mod_a_in_group_1 != mod_b_in_group_1,
        "mod_a and mod_b must be in different groups, got:\n{refs_section}"
    );

    // Split on group boundaries to get per-group content
    let group_1_content = refs_section
        .split("load_config (1):")
        .nth(1)
        .and_then(|s| s.split("load_config (2):").next())
        .unwrap_or("");

    let group_2_content = refs_section.split("load_config (2):").nth(1).unwrap_or("");

    let (mod_a_refs, mod_b_refs) = if mod_a_in_group_1 {
        (group_1_content, group_2_content)
    } else {
        (group_2_content, group_1_content)
    };

    // caller_c.mock imported from mod_a — must be in mod_a's group
    assert!(
        mod_a_refs.contains(&format!("caller_c.{MOCK_LANG_A}")),
        "caller_c.mock should be in mod_a's group, got:\n{mod_a_refs}"
    );
    // caller_d.mock imported from mod_b — must be in mod_b's group
    assert!(
        mod_b_refs.contains(&format!("caller_d.{MOCK_LANG_A}")),
        "caller_d.mock should be in mod_b's group, got:\n{mod_b_refs}"
    );
    // Negative: callers must NOT appear in the wrong group
    assert!(
        !mod_a_refs.contains(&format!("caller_d.{MOCK_LANG_A}")),
        "caller_d.mock should NOT be in mod_a's group, got:\n{mod_a_refs}"
    );
    assert!(
        !mod_b_refs.contains(&format!("caller_c.{MOCK_LANG_A}")),
        "caller_c.mock should NOT be in mod_b's group, got:\n{mod_b_refs}"
    );

    Ok(())
}

/// Capstone test: exercises all three search tiers with a fixture that
/// requires correct hover, references, and disambiguation to pass.
///
/// Guards against:
/// - `handle_hover` returning keyword instead of symbol name
/// - `handle_references` searching for keyword instead of symbol name
/// - disambiguation grouping references under wrong definitions
/// - file match dedup failing to subtract reference lines
#[test]
#[allow(
    clippy::too_many_lines,
    clippy::similar_names,
    reason = "capstone test requires exhaustive assertions across all three search tiers"
)]
fn test_search_full_payload() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Two definitions with the same name in different files
    let mod_a = dir.path().join(format!("mod_a.{MOCK_LANG_A}"));
    std::fs::write(&mod_a, "fn load_config()\n")?;

    let mod_b = dir.path().join(format!("mod_b.{MOCK_LANG_A}"));
    std::fs::write(&mod_b, "fn load_config()\n")?;

    // Two callers, each scoped to one definition via import
    let caller_c = dir.path().join(format!("caller_c.{MOCK_LANG_A}"));
    std::fs::write(
        &caller_c,
        "from mod_a import load_config\nfn use_a()\n  load_config\n",
    )?;

    let caller_d = dir.path().join(format!("caller_d.{MOCK_LANG_A}"));
    std::fs::write(
        &caller_d,
        "from mod_b import load_config\nfn use_b()\n  load_config\n",
    )?;

    // Non-code file: .txt is not indexed by scan_roots (only .mock),
    // so it stays out of LSP references — exercising File matches.
    let notes = dir.path().join("notes.txt");
    std::fs::write(
        &notes,
        "The load_config function handles configuration loading.\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": { "queries": ["load_config"] }
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

    // ── Symbols tier ──────────────────────────────────────────────

    assert!(
        text.contains("## Symbols"),
        "Expected Symbols section, got:\n{text}"
    );

    // Both definitions appear with correct kind
    let function_count = text.matches("[Function]").count();
    assert!(
        function_count >= 2,
        "Expected at least 2 [Function] symbols, found {function_count} in:\n{text}"
    );

    // Both definition files referenced in symbols
    assert!(
        text.contains(&format!("mod_a.{MOCK_LANG_A}")),
        "Expected mod_a.mock in output, got:\n{text}"
    );
    assert!(
        text.contains(&format!("mod_b.{MOCK_LANG_A}")),
        "Expected mod_b.mock in output, got:\n{text}"
    );

    // ── Hover content (guards against handle_hover bug) ───────────
    //
    // Extract the Symbols section (before References).
    // With the fix: hover returns "load_config" → rendered in code block.
    // Without the fix: hover returns "fn" → bare keyword.
    let symbols_section = text.split("## References").next().unwrap_or(text);

    // Indented hover lines (2-space indent, not 4-space which is
    // call hierarchy content)
    let hover_lines: Vec<&str> = symbols_section
        .lines()
        .filter(|l| l.starts_with("  ") && !l.starts_with("    "))
        .filter(|l| l.trim() != "```")
        .collect();

    // At least one hover line should contain the symbol name
    assert!(
        hover_lines.iter().any(|l| l.contains("load_config")),
        "Hover should contain 'load_config', got hover lines: {hover_lines:?}"
    );

    // No hover line should be just the keyword
    assert!(
        !hover_lines.iter().any(|l| l.trim() == "fn"),
        "Hover should not be bare keyword 'fn', got hover lines: {hover_lines:?}"
    );

    // ── References tier ───────────────────────────────────────────

    assert!(
        text.contains("## References"),
        "Expected References section, got:\n{text}"
    );

    // ── Disambiguation groups (guards against references bug) ─────
    //
    // Both groups must render.
    assert!(
        text.contains("load_config (1):"),
        "Expected disambiguation group (1), got:\n{text}"
    );
    assert!(
        text.contains("load_config (2):"),
        "Expected disambiguation group (2), got:\n{text}"
    );

    // ── Caller routing (guards against scope resolution) ──────────
    //
    // Extract the References section.
    let refs_section = text
        .split("## References")
        .nth(1)
        .and_then(|s| s.split("## File matches").next())
        .unwrap_or("");

    // Determine which group number each definition got.
    let mod_a_in_grp1 = refs_section.contains(&format!("mod_a.{MOCK_LANG_A} load_config (1):"));
    let mod_a_in_grp2 = refs_section.contains(&format!("mod_a.{MOCK_LANG_A} load_config (2):"));
    assert!(
        mod_a_in_grp1 || mod_a_in_grp2,
        "mod_a should appear in a group header, got:\n{refs_section}"
    );

    let mod_b_in_grp1 = refs_section.contains(&format!("mod_b.{MOCK_LANG_A} load_config (1):"));
    let mod_b_in_grp2 = refs_section.contains(&format!("mod_b.{MOCK_LANG_A} load_config (2):"));
    assert!(
        mod_b_in_grp1 || mod_b_in_grp2,
        "mod_b should appear in a group header, got:\n{refs_section}"
    );

    // They must be in different groups
    assert!(
        mod_a_in_grp1 != mod_b_in_grp1,
        "mod_a and mod_b must be in different groups, got:\n{refs_section}"
    );

    // Split on group boundaries to get per-group content
    let grp1_content = refs_section
        .split("load_config (1):")
        .nth(1)
        .and_then(|s| s.split("load_config (2):").next())
        .unwrap_or("");

    let grp2_content = refs_section.split("load_config (2):").nth(1).unwrap_or("");

    let (mod_a_refs, mod_b_refs) = if mod_a_in_grp1 {
        (grp1_content, grp2_content)
    } else {
        (grp2_content, grp1_content)
    };

    // caller_c.mock imported from mod_a → must be in mod_a's group
    assert!(
        mod_a_refs.contains(&format!("caller_c.{MOCK_LANG_A}")),
        "caller_c.mock should be in mod_a's group, got:\n{mod_a_refs}"
    );

    // caller_d.mock imported from mod_b → must be in mod_b's group
    assert!(
        mod_b_refs.contains(&format!("caller_d.{MOCK_LANG_A}")),
        "caller_d.mock should be in mod_b's group, got:\n{mod_b_refs}"
    );

    // Negative: callers must NOT appear in the wrong group
    assert!(
        !mod_a_refs.contains(&format!("caller_d.{MOCK_LANG_A}")),
        "caller_d.mock should NOT be in mod_a's group, got:\n{mod_a_refs}"
    );
    assert!(
        !mod_b_refs.contains(&format!("caller_c.{MOCK_LANG_A}")),
        "caller_c.mock should NOT be in mod_b's group, got:\n{mod_b_refs}"
    );

    // ── File matches tier ─────────────────────────────────────────
    //
    // notes.txt is not a .mock file, so scan_roots doesn't index it.
    // Ripgrep finds it but LSP references don't cover it — it appears
    // in File matches.
    assert!(
        text.contains("## File matches"),
        "Expected File matches section, got:\n{text}"
    );
    assert!(
        text.contains("notes.txt"),
        "Expected notes.txt in file matches, got:\n{text}"
    );

    // Code files should NOT appear in File matches — their lines
    // are already in References and should be deduped out.
    let file_matches_section = text.split("## File matches").nth(1).unwrap_or("");

    assert!(
        !file_matches_section.contains(&format!("mod_a.{MOCK_LANG_A}")),
        "mod_a.mock should be deduped from file matches, got:\n{file_matches_section}"
    );
    assert!(
        !file_matches_section.contains(&format!("caller_c.{MOCK_LANG_A}")),
        "caller_c.mock should be deduped from file matches, got:\n{file_matches_section}"
    );

    Ok(())
}

/// Verifies cross-language search disambiguation: two mockls instances
/// serving different languages each define `fn perform_task()`. Search
/// should return both symbols and group their references by language.
#[test]
#[allow(
    clippy::too_many_lines,
    clippy::similar_names,
    reason = "cross-language disambiguation test requires thorough assertions"
)]
fn test_search_cross_language_disambiguation() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let file_a = dir.path().join(format!("mod_a.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn perform_task()\nperform_task\n")?;

    let file_b = dir.path().join(format!("mod_b.{MOCK_LANG_B}"));
    std::fs::write(&file_b, "fn perform_task()\nperform_task\n")?;

    let lsp_a = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let lsp_b = mockls_lsp_arg(MOCK_LANG_B, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp_a, &lsp_b], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": { "queries": ["perform_task"] }
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

    // Both symbols should appear
    assert!(
        text.contains("## Symbols"),
        "Expected Symbols section, got:\n{text}"
    );
    assert!(
        text.contains(&format!("mod_a.{MOCK_LANG_A}")),
        "Expected mod_a.{MOCK_LANG_A} in output, got:\n{text}"
    );
    assert!(
        text.contains(&format!("mod_b.{MOCK_LANG_B}")),
        "Expected mod_b.{MOCK_LANG_B} in output, got:\n{text}"
    );

    // Disambiguation groups must exist
    assert!(
        text.contains("## References"),
        "Expected References section for cross-language symbols, got:\n{text}"
    );
    assert!(
        text.contains("perform_task (1):"),
        "Expected disambiguation group (1), got:\n{text}"
    );
    assert!(
        text.contains("perform_task (2):"),
        "Expected disambiguation group (2), got:\n{text}"
    );

    // Each group's references should stay within its own language file
    let refs_section = text
        .split("## References")
        .nth(1)
        .and_then(|s| s.split("## File matches").next())
        .unwrap_or("");

    let grp1_content = refs_section
        .split("perform_task (1):")
        .nth(1)
        .and_then(|s| s.split("perform_task (2):").next())
        .unwrap_or("");

    let grp2_content = refs_section.split("perform_task (2):").nth(1).unwrap_or("");

    let mod_a_in_grp1 = grp1_content.contains(&format!("mod_a.{MOCK_LANG_A}"));
    let mod_b_in_grp2 = grp2_content.contains(&format!("mod_b.{MOCK_LANG_B}"));
    let mod_b_in_grp1 = grp1_content.contains(&format!("mod_b.{MOCK_LANG_B}"));
    let mod_a_in_grp2 = grp2_content.contains(&format!("mod_a.{MOCK_LANG_A}"));

    // Each file should appear in exactly one group
    assert!(
        (mod_a_in_grp1 && mod_b_in_grp2) || (mod_b_in_grp1 && mod_a_in_grp2),
        "Each language's file should be in a different group. \
         grp1: {grp1_content}\ngrp2: {grp2_content}"
    );

    Ok(())
}

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

/// Find line and column (0-indexed) of a substring
fn find_position(content: &str, substring: &str) -> Result<(u32, u32)> {
    for (line_idx, line) in content.lines().enumerate() {
        if let Some(col_idx) = line.find(substring) {
            let line_u32 = u32::try_from(line_idx).context("line index overflow")?;
            let col_u32 = u32::try_from(col_idx).context("column index overflow")?;
            return Ok((line_u32, col_u32));
        }
    }
    Err(anyhow!("Substring '{substring}' not found in content"))
}

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
    let lsp = mockls_lsp_arg("shellscript", "");
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
    let lsp = mockls_lsp_arg("shellscript", "");
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

    // Check all expected tools are present
    let expected_tools = [
        "definition",
        "type_definition",
        "implementation",
        "find_references",
        "document_symbols",
        "search",
        "diagnostics",
        "call_hierarchy",
        "type_hierarchy",
        "status",
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
fn test_mcp_tool_call_invalid_file() -> Result<()> {
    let lsp = mockls_lsp_arg("shellscript", "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], "/tmp")?;
    bridge.initialize()?;

    // Request definition on non-existent file
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "name": "definition",
            "arguments": {
                "file": "/tmp/nonexistent_file_12345.sh",
                "line": 0,
                "character": 0
            }
        }
    }))?;

    let response = bridge.recv()?;

    assert!(response.get("result").is_some());

    let result = &response["result"];
    // Should return an error result
    assert_eq!(
        result["isError"], true,
        "Expected error for nonexistent file"
    );
    Ok(())
}

#[test]
fn test_mcp_tool_call_unknown_tool() -> Result<()> {
    let lsp = mockls_lsp_arg("shellscript", "");
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
    let lsp = mockls_lsp_arg("shellscript", "");
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
    let lsp = mockls_lsp_arg("shellscript", "");
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
fn test_catenary_find_references_by_position() -> Result<()> {
    // Create a test script with a function that's called multiple times
    let test_file = "/tmp/mcp_test_find_refs.sh";
    std::fs::write(
        test_file,
        r#"#!/bin/bash

my_func() {
    echo "hello"
}

my_func
my_func
"#,
    )?;

    let lsp = mockls_lsp_arg("shellscript", "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], "/tmp")?;
    bridge.initialize()?;

    // Request references by position (on the function definition, line 2)
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 500,
        "method": "tools/call",
        "params": {
            "name": "find_references",
            "arguments": {
                "file": test_file,
                "line": 2,
                "character": 0
            }
        }
    }))?;

    let response = bridge.recv()?;

    assert!(
        response.get("result").is_some(),
        "Find references call failed: {response:?}"
    );

    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Expected success, got error: {result:?}"
    );

    let content_arr = result["content"]
        .as_array()
        .context("Missing content array")?;
    let text = content_arr[0]["text"]
        .as_str()
        .context("Missing text in content")?;

    // Should find at least the definition and calls
    // The definition should be marked with [def]
    assert!(
        text.contains("[def]"),
        "Expected definition marker [def] in output, got: {text}"
    );

    std::fs::remove_file(test_file).ok();
    Ok(())
}

#[test]
fn test_catenary_find_references_by_symbol() -> Result<()> {
    // Create a test script with a function
    let test_file = "/tmp/mcp_test_find_refs_symbol.sh";
    std::fs::write(
        test_file,
        r#"#!/bin/bash

unique_test_func() {
    echo "hello"
}

unique_test_func
"#,
    )?;

    let lsp = mockls_lsp_arg("shellscript", "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], "/tmp")?;
    bridge.initialize()?;

    // Request references by symbol name
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 501,
        "method": "tools/call",
        "params": {
            "name": "find_references",
            "arguments": {
                "symbol": "unique_test_func",
                "file": test_file
            }
        }
    }))?;

    let response = bridge.recv()?;

    let result = &response["result"];

    // mockls workspace/symbol only returns results for opened documents.
    // The find_references tool may or may not open the file before the
    // workspace/symbol lookup, so accept either success or "not found".
    let content_arr = result["content"]
        .as_array()
        .context("Missing content array")?;
    if result["isError"] == true {
        let text = content_arr[0]["text"]
            .as_str()
            .context("Missing text in content")?;
        assert!(
            text.contains("not found") || text.contains("No references"),
            "Unexpected error: {text}"
        );
    } else if !content_arr.is_empty() {
        let text = content_arr[0]["text"]
            .as_str()
            .context("Missing text in content")?;
        assert!(
            text.contains(test_file) || text.contains("No references"),
            "Expected references to contain file path, got: {text}"
        );
    }

    std::fs::remove_file(test_file).ok();
    Ok(())
}

#[test]
fn test_catenary_find_references_missing_args() -> Result<()> {
    let lsp = mockls_lsp_arg("shellscript", "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], "/tmp")?;
    bridge.initialize()?;

    // Request without symbol or file - should fail
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 502,
        "method": "tools/call",
        "params": {
            "name": "find_references",
            "arguments": {}
        }
    }))?;

    let response = bridge.recv()?;

    let result = &response["result"];
    assert_eq!(
        result["isError"], true,
        "Expected error for missing arguments"
    );
    Ok(())
}

#[test]
fn test_multi_root_find_symbol() -> Result<()> {
    // Create two roots with unique function names
    let dir_a = tempfile::tempdir().context("Failed to create temp dir A")?;
    let dir_b = tempfile::tempdir().context("Failed to create temp dir B")?;

    let script_a = dir_a.path().join("alpha.sh");
    std::fs::write(
        &script_a,
        "#!/bin/bash\nfunction alpha_func() { echo alpha; }\n",
    )?;

    let script_b = dir_b.path().join("beta.sh");
    std::fs::write(
        &script_b,
        "#!/bin/bash\nfunction beta_func() { echo beta; }\n",
    )?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    let lsp = mockls_lsp_arg("shellscript", "");
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
        text_a.contains("alpha.sh"),
        "Expected search to find alpha.sh, got: {text_a}"
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
        text_b.contains("beta.sh"),
        "Expected search to find beta.sh, got: {text_b}"
    );

    Ok(())
}

#[test]
fn test_multi_root_definition() -> Result<()> {
    // Create two roots, each with a function definition and a call
    let dir_a = tempfile::tempdir().context("Failed to create temp dir A")?;
    let dir_b = tempfile::tempdir().context("Failed to create temp dir B")?;

    let script_a = dir_a.path().join("defs_a.sh");
    std::fs::write(
        &script_a,
        "#!/bin/bash\nfunction greet_a() { echo hi; }\ngreet_a\n",
    )?;

    let script_b = dir_b.path().join("defs_b.sh");
    std::fs::write(
        &script_b,
        "#!/bin/bash\nfunction greet_b() { echo hi; }\ngreet_b\n",
    )?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    let lsp = mockls_lsp_arg("shellscript", "");
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp], &[root_a, root_b])?;
    bridge.initialize()?;

    // Go to definition of greet_a call in root A (line 2, char 0)
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 710,
        "method": "tools/call",
        "params": {
            "name": "definition",
            "arguments": {
                "file": script_a.to_str().context("Invalid script A path")?,
                "line": 2,
                "character": 0
            }
        }
    }))?;

    let response_a = bridge.recv()?;
    let result_a = &response_a["result"];
    assert!(
        result_a["isError"].is_null() || result_a["isError"] == false,
        "Definition in root A failed: {response_a:?}"
    );
    let text_a = result_a["content"][0]["text"]
        .as_str()
        .context("Missing text for definition A")?;
    assert!(
        text_a.contains("defs_a.sh"),
        "Definition should point to defs_a.sh, got: {text_a}"
    );

    // Go to definition of greet_b call in root B (line 2, char 0)
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 711,
        "method": "tools/call",
        "params": {
            "name": "definition",
            "arguments": {
                "file": script_b.to_str().context("Invalid script B path")?,
                "line": 2,
                "character": 0
            }
        }
    }))?;

    let response_b = bridge.recv()?;
    let result_b = &response_b["result"];
    assert!(
        result_b["isError"].is_null() || result_b["isError"] == false,
        "Definition in root B failed: {response_b:?}"
    );
    let text_b = result_b["content"][0]["text"]
        .as_str()
        .context("Missing text for definition B")?;
    assert!(
        text_b.contains("defs_b.sh"),
        "Definition should point to defs_b.sh, got: {text_b}"
    );

    Ok(())
}

#[test]
fn test_multi_root_document_symbols() -> Result<()> {
    // Create two roots with different symbols
    let dir_a = tempfile::tempdir().context("Failed to create temp dir A")?;
    let dir_b = tempfile::tempdir().context("Failed to create temp dir B")?;

    let script_a = dir_a.path().join("syms_a.sh");
    std::fs::write(
        &script_a,
        "#!/bin/bash\nfunction sym_alpha() { echo a; }\nfunction sym_beta() { echo b; }\n",
    )?;

    let script_b = dir_b.path().join("syms_b.sh");
    std::fs::write(
        &script_b,
        "#!/bin/bash\nfunction sym_gamma() { echo c; }\nfunction sym_delta() { echo d; }\n",
    )?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    let lsp = mockls_lsp_arg("shellscript", "");
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

    let script_a = dir_a.path().join("funcs_a.sh");
    std::fs::write(
        &script_a,
        "#!/bin/bash\nfunction unique_root_a_func() { echo a; }\nunique_root_a_func\n",
    )?;

    let script_b = dir_b.path().join("funcs_b.sh");
    std::fs::write(
        &script_b,
        "#!/bin/bash\nfunction unique_root_b_func() { echo b; }\nunique_root_b_func\n",
    )?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    // Spawn bridge with only root_a
    let lsp = mockls_lsp_arg("shellscript", "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root_a)?;
    bridge.initialize_with_roots(&[root_a])?;

    // Definition on function call in root_a — server should be working
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "definition",
            "arguments": {
                "file": script_a.to_str().context("Invalid script A path")?,
                "line": 2,
                "character": 0
            }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Definition on root A function failed: {response:?}"
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

    // Definition in root_b — server should have been restarted with new roots.
    // Retry loop to accommodate spawn + initialize time.
    let mut success = false;
    for i in 0..10 {
        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": 20 + i,
            "method": "tools/call",
            "params": {
                "name": "definition",
                "arguments": {
                    "file": script_b.to_str().context("Invalid script B path")?,
                    "line": 2,
                    "character": 0
                }
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
        if text.contains("funcs_b.sh") {
            success = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    assert!(
        success,
        "Definition in root B should succeed after server restart with new roots"
    );

    Ok(())
}

// ─── roots/list tests ───────────────────────────────────────────────────

#[test]
fn test_roots_list_after_initialize() -> Result<()> {
    let lsp = mockls_lsp_arg("shellscript", "");
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
    let lsp = mockls_lsp_arg("shellscript", "");
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
    let lsp = mockls_lsp_arg("shellscript", "");
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
        format!("{lang}:{bin}")
    } else {
        format!("{lang}:{bin} {flags}")
    }
}

#[test]
fn test_mockls_definition_across_profiles() -> Result<()> {
    let profiles: &[(&str, &str)] = &[("clean", ""), ("workspace-folders", "--workspace-folders")];

    for (name, flags) in profiles {
        let test_file = "/tmp/mockls_def_test.sh";
        std::fs::write(
            test_file,
            "#!/bin/bash\nfn my_function() { echo hi; }\nmy_function\n",
        )?;

        let lsp = mockls_lsp_arg("shellscript", flags);
        let mut bridge = BridgeProcess::spawn(&[&lsp], "/tmp")?;
        bridge.initialize()?;

        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "definition",
                "arguments": {
                    "file": test_file,
                    "line": 2,
                    "character": 0
                }
            }
        }))?;

        let response = bridge.recv()?;
        let result = &response["result"];
        assert!(
            result["isError"].is_null() || result["isError"] == false,
            "Profile {name}: definition failed: {response:?}"
        );

        let content = result["content"]
            .as_array()
            .context(format!("Profile {name}: missing content array"))?;
        assert!(
            !content.is_empty(),
            "Profile {name}: expected definition content"
        );

        let text = content[0]["text"]
            .as_str()
            .context(format!("Profile {name}: missing text"))?;
        assert!(
            text.contains(test_file),
            "Profile {name}: definition should contain file path, got: {text}"
        );

        std::fs::remove_file(test_file).ok();
    }
    Ok(())
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
        let test_file = "/tmp/mockls_diag_test.sh";
        std::fs::write(test_file, "#!/bin/bash\necho hello\n")?;

        let lsp = mockls_lsp_arg("shellscript", flags);
        let mut bridge = BridgeProcess::spawn(&[&lsp], "/tmp")?;
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

        std::fs::remove_file(test_file).ok();
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
        ("no-workspace-folders", ""),
        ("workspace-folders", "--workspace-folders"),
    ];

    for (name, flags) in profiles {
        let dir_a = tempfile::tempdir().context("Failed to create temp dir A")?;
        let dir_b = tempfile::tempdir().context("Failed to create temp dir B")?;

        let script_a = dir_a.path().join("funcs_a.sh");
        std::fs::write(
            &script_a,
            "#!/bin/bash\nfn unique_root_a_func() { echo a; }\nunique_root_a_func\n",
        )?;

        let script_b = dir_b.path().join("funcs_b.sh");
        std::fs::write(
            &script_b,
            "#!/bin/bash\nfn unique_root_b_func() { echo b; }\nunique_root_b_func\n",
        )?;

        let root_a = dir_a.path().to_str().context("Invalid path A")?;
        let root_b = dir_b.path().to_str().context("Invalid path B")?;

        let lsp = mockls_lsp_arg("shellscript", flags);
        let mut bridge = BridgeProcess::spawn(&[&lsp], root_a)?;
        bridge.initialize_with_roots(&[root_a])?;

        // Definition on function call in root_a
        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "definition",
                "arguments": {
                    "file": script_a.to_str().context("Invalid script A path")?,
                    "line": 2,
                    "character": 0
                }
            }
        }))?;

        let response = bridge.recv()?;
        let result = &response["result"];
        assert!(
            result["isError"].is_null() || result["isError"] == false,
            "Profile {name}: definition on root A failed: {response:?}"
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

        // Definition on function call in root_b — bridge blocks until ready
        // (via wait_for_server_ready: activity settle for no-progress mockls,
        // or after restart for servers without workspace folder support)
        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "tools/call",
            "params": {
                "name": "definition",
                "arguments": {
                    "file": script_b.to_str().context("Invalid script B path")?,
                    "line": 2,
                    "character": 0
                }
            }
        }))?;

        let response = bridge.recv()?;
        let result = &response["result"];
        assert!(
            result["isError"] != true,
            "Profile {name}: definition on root B returned error: {response:?}"
        );
        let text = result["content"][0]["text"]
            .as_str()
            .context("Missing text")?;
        assert!(
            text.contains("funcs_b.sh"),
            "Profile {name}: definition on root B should reference funcs_b.sh, got: {text}"
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

    let file_a = dir_a.path().join("funcs_a.sh");
    std::fs::write(&file_a, "#!/bin/bash\nfn hello() { echo hi; }\nhello\n")?;
    let file_b = dir_b.path().join("funcs_b.sh");
    std::fs::write(&file_b, "#!/bin/bash\nfn world() { echo world; }\nworld\n")?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    // mockls with --workspace-folders but NO --indexing-delay:
    // supports didChangeWorkspaceFolders, never sends $/progress.
    let lsp = mockls_lsp_arg("shellscript", "--workspace-folders");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root_a)?;
    bridge.initialize_with_roots(&[root_a])?;

    // Definition on root_a — establishes server is working
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "definition",
            "arguments": {
                "file": file_a.to_str().context("Invalid file A path")?,
                "line": 2,
                "character": 0
            }
        }
    }))?;

    let response = bridge.recv()?;
    assert!(
        response["result"]["isError"] != true,
        "Root A definition failed: {response:?}"
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

    // Definition on root_b — must not hang.
    // did_change_workspace_folders sets state to Busy.
    // Since mockls never sends $/progress, wait_ready() uses
    // the activity settle fallback to transition back to Ready.
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 20,
        "method": "tools/call",
        "params": {
            "name": "definition",
            "arguments": {
                "file": file_b.to_str().context("Invalid file B path")?,
                "line": 2,
                "character": 0
            }
        }
    }))?;

    let response = bridge.recv()?;
    assert!(
        response["result"]["isError"] != true,
        "Root B definition should not hang or error: {response:?}"
    );
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing definition text for root B")?;
    assert!(
        text.contains("funcs_b.sh"),
        "Expected 'funcs_b.sh' in definition, got: {text}"
    );

    Ok(())
}

#[test]
fn test_mockls_multiplexing() -> Result<()> {
    // Spawn two mockls instances as different languages
    let dir = tempfile::tempdir()?;

    let shell_file = dir.path().join("test.sh");
    std::fs::write(&shell_file, "#!/bin/bash\nfn greet() { echo hi; }\ngreet\n")?;

    let toml_file = dir.path().join("test.toml");
    std::fs::write(&toml_file, "[package]\nname = \"test\"\n")?;

    let lsp_shell = mockls_lsp_arg("shellscript", "");
    let lsp_toml = mockls_lsp_arg("toml", "");
    let root = dir.path().to_str().context("Invalid root path")?;

    let mut bridge = BridgeProcess::spawn(&[&lsp_shell, &lsp_toml], root)?;
    bridge.initialize()?;

    // Definition on shell file
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 100,
        "method": "tools/call",
        "params": {
            "name": "definition",
            "arguments": {
                "file": shell_file.to_str().context("Invalid shell file path")?,
                "line": 2,
                "character": 0
            }
        }
    }))?;

    let response_shell = bridge.recv()?;
    let result_shell = &response_shell["result"];
    assert!(
        result_shell["isError"].is_null() || result_shell["isError"] == false,
        "Shell definition failed: {response_shell:?}"
    );

    // Definition on TOML file
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 101,
        "method": "tools/call",
        "params": {
            "name": "definition",
            "arguments": {
                "file": toml_file.to_str().context("Invalid toml file path")?,
                "line": 1,
                "character": 0
            }
        }
    }))?;

    let response_toml = bridge.recv()?;
    let result_toml = &response_toml["result"];
    assert!(
        result_toml["isError"].is_null() || result_toml["isError"] == false,
        "TOML definition failed: {response_toml:?}"
    );

    // Verify different servers responded (both should return content)
    let text_shell = result_shell["content"][0]["text"]
        .as_str()
        .context("Missing shell definition text")?;
    let text_toml = result_toml["content"][0]["text"]
        .as_str()
        .context("Missing toml definition text")?;

    assert!(
        text_shell.contains("test.sh"),
        "Shell definition should reference test.sh, got: {text_shell}"
    );
    assert!(
        text_toml.contains("test.toml"),
        "TOML definition should reference test.toml, got: {text_toml}"
    );

    Ok(())
}

/// Verifies that Catenary does NOT send `didSave` when the server does not
/// advertise `textDocumentSync.save` (Gap 2 negative case).
#[test]
fn test_mockls_did_save_not_sent_without_capability() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    let test_file = dir.path().join("test.sh");
    std::fs::write(&test_file, "#!/bin/bash\necho hello\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        "shellscript",
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
    let test_file = dir.path().join("test.sh");
    std::fs::write(&test_file, "#!/bin/bash\necho hello\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        "shellscript",
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

/// Verifies that `ContentModified` (-32801) is retried transparently (Gap 5).
/// mockls `--content-modified-once` returns `ContentModified` on the first
/// definition request, then succeeds on retry.
#[test]
fn test_mockls_content_modified_retry() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let test_file = dir.path().join("test.sh");
    let content = "#!/bin/bash\nfn greet() { echo hi; }\ngreet\n";
    std::fs::write(&test_file, content)?;

    let lsp = mockls_lsp_arg("shellscript", "--content-modified-once");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let (line, col) = find_position(content, "greet")?;
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "definition",
            "arguments": {
                "file": test_file.to_str().context("file path")?,
                "line": line,
                "character": col
            }
        }
    }))?;

    let response = bridge.recv()?;
    assert!(
        response["result"]["isError"] != true,
        "Definition should succeed after ContentModified retry: {response:?}"
    );
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing definition text")?;
    assert!(
        text.contains("test.sh"),
        "Definition result should contain file path, got: {text}"
    );

    Ok(())
}

/// Verifies that a hanging LSP request returns an error instead of blocking
/// forever (Gap 4). mockls `--hang-on textDocument/definition` never responds
/// to definition requests. Catenary's 30s wall-clock backstop should fire.
#[test]
fn test_mockls_request_hang_detection() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let test_file = dir.path().join("test.sh");
    std::fs::write(&test_file, "#!/bin/bash\necho hello\n")?;

    let lsp = mockls_lsp_arg("shellscript", "--hang-on textDocument/definition");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "definition",
            "arguments": {
                "file": test_file.to_str().context("file path")?,
                "line": 1,
                "character": 0
            }
        }
    }))?;

    let response = bridge.recv()?;

    // Should return an error, not hang forever
    let result = &response["result"];
    assert_eq!(
        result["isError"], true,
        "Hanging request should return error, got: {response:?}"
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
    let test_file = dir.path().join("test.sh");
    std::fs::write(&test_file, "#!/bin/bash\necho hello\n")?;

    let dir2 = tempfile::tempdir()?;

    let lsp = mockls_lsp_arg(
        "shellscript",
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

    // Send a definition request — wait_ready should detect failure
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "definition",
            "arguments": {
                "file": test_file.to_str().context("file path")?,
                "line": 1,
                "character": 0
            }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert_eq!(
        result["isError"], true,
        "wait_ready should detect failure when server burns CPU without progress. Got: {response:?}"
    );

    Ok(())
}

/// Verifies warmup observation: `is_ready()` waits for the server to
/// become Sleeping before declaring it ready (Gap 6).
///
/// mockls `--cpu-on-initialized 3000` burns 3s of CPU on `initialized`.
/// During warmup (<3s from spawn), the server is Running, so `is_ready()`
/// returns false. After the burn completes, the server goes Sleeping,
/// `is_ready()` returns true, and the definition request succeeds.
#[test]
fn test_warmup_observation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let test_file = dir.path().join("test.sh");
    std::fs::write(
        &test_file,
        "#!/bin/bash\nfn my_function() { echo hi; }\nmy_function\n",
    )?;

    let lsp = mockls_lsp_arg("shellscript", "--cpu-on-initialized 3000");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Send definition immediately — server is still burning CPU
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "definition",
            "arguments": {
                "file": test_file.to_str().context("file path")?,
                "line": 2,
                "character": 0
            }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    // Should succeed — wait_ready waits for CPU burn to finish
    let text = result["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("test.sh"),
        "Definition should succeed after warmup observation. Got: {text}"
    );

    Ok(())
}

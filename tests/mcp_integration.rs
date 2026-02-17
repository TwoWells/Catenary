#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! End-to-end integration tests for the MCP-LSP bridge.
//!
//! These tests spawn the actual bridge binary and communicate with it
//! via stdin/stdout using the MCP protocol.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

/// Check if a command exists in PATH
fn command_exists(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

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

macro_rules! require_bash_lsp {
    () => {
        if !command_exists("bash-language-server") {
            tracing::warn!("Skipping test: bash-language-server not installed");
            return Ok(());
        }
    };
}

macro_rules! require_taplo {
    () => {
        if !command_exists("taplo") {
            tracing::warn!("Skipping test: taplo not installed");
            return Ok(());
        }
    };
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

        // Give LSP server time to initialize
        std::thread::sleep(Duration::from_millis(500));

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
    require_bash_lsp!();

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp")?;

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
    require_bash_lsp!();

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp")?;
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
        "hover",
        "definition",
        "type_definition",
        "implementation",
        "find_references",
        "document_symbols",
        "search",
        "code_actions",
        "rename",
        "completion",
        "diagnostics",
        "signature_help",
        "formatting",
        "range_formatting",
        "call_hierarchy",
        "type_hierarchy",
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
fn test_mcp_hover_builtin() -> Result<()> {
    require_bash_lsp!();

    // Create a test script
    let test_file = "/tmp/mcp_test_hover.sh";
    std::fs::write(test_file, "#!/bin/bash\necho \"hello\"\n")?;

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp")?;
    bridge.initialize()?;

    // Request hover on 'echo' (line 1, character 0)
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "hover",
            "arguments": {
                "file": test_file,
                "line": 1,
                "character": 0
            }
        }
    }))?;

    let response = bridge.recv()?;

    assert!(
        response.get("result").is_some(),
        "Hover call failed: {response:?}"
    );

    let result = &response["result"];
    assert!(result["isError"].is_null() || result["isError"] == false);

    let content = result["content"]
        .as_array()
        .context("Missing content array")?;
    assert!(!content.is_empty(), "Expected hover content");

    let text = content[0]["text"]
        .as_str()
        .context("Missing text in content")?;
    assert!(
        text.contains("echo"),
        "Hover should contain 'echo' documentation, got: {text}"
    );

    std::fs::remove_file(test_file).ok();
    Ok(())
}

#[test]
fn test_mcp_definition() -> Result<()> {
    require_bash_lsp!();

    // Create a test script with a function definition and call
    let test_file = "/tmp/mcp_test_definition.sh";
    std::fs::write(
        test_file,
        r#"#!/bin/bash

my_function() {
    echo "hello"
}

my_function
"#,
    )?;

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp")?;
    bridge.initialize()?;

    // Request definition on 'my_function' call (line 6, character 0)
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "definition",
            "arguments": {
                "file": test_file,
                "line": 6,
                "character": 0
            }
        }
    }))?;

    let response = bridge.recv()?;

    assert!(
        response.get("result").is_some(),
        "Definition call failed: {response:?}"
    );

    let result = &response["result"];
    assert!(result["isError"].is_null() || result["isError"] == false);

    let content = result["content"]
        .as_array()
        .context("Missing content array")?;
    assert!(!content.is_empty(), "Expected definition content");

    let text = content[0]["text"]
        .as_str()
        .context("Missing text in content")?;
    // Should point to line 3 where my_function is defined
    assert!(
        text.contains(test_file) && text.contains(":3:"),
        "Definition should point to function definition at line 3, got: {text}"
    );

    std::fs::remove_file(test_file).ok();
    Ok(())
}

#[test]
fn test_mcp_hover_no_info() -> Result<()> {
    require_bash_lsp!();

    // Create a test script
    let test_file = "/tmp/mcp_test_hover_empty.sh";
    std::fs::write(test_file, "#!/bin/bash\n# just a comment\n")?;

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp")?;
    bridge.initialize()?;

    // Request hover on comment (line 1, character 0)
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "hover",
            "arguments": {
                "file": test_file,
                "line": 1,
                "character": 2
            }
        }
    }))?;

    let response = bridge.recv()?;

    assert!(response.get("result").is_some());

    let result = &response["result"];
    // Should not be an error, just no hover info
    assert!(result["isError"].is_null() || result["isError"] == false);

    std::fs::remove_file(test_file).ok();
    Ok(())
}

#[test]
fn test_mcp_tool_call_invalid_file() -> Result<()> {
    require_bash_lsp!();

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp")?;
    bridge.initialize()?;

    // Request hover on non-existent file
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "name": "hover",
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
    require_bash_lsp!();

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp")?;
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
    require_bash_lsp!();

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp")?;
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
#[allow(
    clippy::too_many_lines,
    reason = "Complex integration test requires many steps"
)]
fn test_multiplexing() -> Result<()> {
    require_bash_lsp!();
    require_taplo!();
    // We assume rust-analyzer is present if we are developing this

    let root_dir = std::env::current_dir()?;
    let root_str = root_dir.to_str().context("invalid root path")?;
    // Use Catenary's own main.rs which is definitely in the workspace
    let rust_file = root_dir.join("src/main.rs");
    let bash_file = root_dir.join("tests/assets/bash/script.sh");
    // We name this Cargo.toml so Taplo automatically detects the schema.
    // If it were named test.toml, Taplo wouldn't provide rich hover info without extra config.
    let toml_file = root_dir.join("tests/assets/toml/Cargo.toml");

    // Spawn with ALL servers
    let mut bridge = BridgeProcess::spawn(
        &[
            "rust:rust-analyzer",
            "shellscript:bash-language-server start",
            "toml:taplo lsp stdio",
        ],
        root_str,
    )?;

    bridge.initialize()?;

    // 1. Test Rust Hover
    // Find a stable token in src/main.rs, e.g., "println" or "main"
    // Let's use "main" on the first few lines
    let content = std::fs::read_to_string(&rust_file)?;
    let (line, col) = find_position(&content, "fn main")?;

    // Retry loop for server startup and indexing
    let mut success = false;
    for i in 0..20 {
        // Retry for up to 10 seconds (20 * 500ms)
        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": 100 + i,
            "method": "tools/call",
            "params": {
                "name": "hover",
                "arguments": {
                    "file": rust_file.to_str().context("invalid rust path")?,
                    "line": line,
                    "character": col + 3 // hover on 'main'
                }
            }
        }))?;

        let response = bridge.recv()?;
        let result = &response["result"];

        let content_arr = result["content"]
            .as_array()
            .context("Missing content array")?;
        let text = content_arr[0]["text"]
            .as_str()
            .context("Missing text in content")?;
        if result["isError"] == true {
            if text.contains("content modified") || text.contains("No hover information") {
                // Ignore errors during warm-up
            } else {
                // Genuine error
                tracing::error!("Unexpected error: {text}");
            }
        } else if text.contains("No hover information") {
            // Not ready yet
        } else if text.contains("main") {
            success = true;
            break;
        }

        std::thread::sleep(Duration::from_millis(500));
    }

    assert!(success, "Rust hover failed to return info after warmup");

    // 2. Test Bash Hover
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 200,
        "method": "tools/call",
        "params": {
            "name": "hover",
            "arguments": {
                "file": bash_file.to_str().context("invalid bash path")?,
                "line": 2, // echo line
                "character": 4 // echo command
            }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Bash hover failed: {response:?}"
    );

    // 3. Test TOML Hover (Taplo)
    // Content is:
    // [package]
    // name = "test-toml"
    let mut taplo_success = false;
    for i in 0..20 {
        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": 300 + i,
            "method": "tools/call",
            "params": {
                "name": "hover",
                "arguments": {
                    "file": toml_file.to_str().context("invalid toml path")?,
                    "line": 1, // name = ...
                    "character": 0 // name
                }
            }
        }))?;

        let response = bridge.recv()?;
        let result = &response["result"];

        let content_arr = result["content"]
            .as_array()
            .context("Missing content array")?;
        let text = content_arr[0]["text"]
            .as_str()
            .context("Missing text in content")?;
        if result["isError"] == true {
            // Taplo might timeout on first request while spawning
            if text.contains("timed out") {
                // retry
            } else {
                tracing::error!("Unexpected TOML error: {text}");
            }
        } else {
            // Taplo usually gives info about the schema field "name"
            if text.contains("name") || text.contains("package") {
                taplo_success = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    assert!(
        taplo_success,
        "TOML hover failed to return info after warmup"
    );

    // 4. Test Find Symbol (replaces workspace symbols)
    std::thread::sleep(Duration::from_secs(1));

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 400,
        "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": {
                "queries": ["greet"]
            }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Find symbol failed"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text in content")?;

    assert!(text.contains("greet"), "Expected to find 'greet' symbol");
    Ok(())
}

#[test]
fn test_client_info_stored_in_session() -> Result<()> {
    require_bash_lsp!();

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp")?;

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
    require_bash_lsp!();

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

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp")?;
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
    require_bash_lsp!();

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

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp")?;
    bridge.initialize()?;

    // Give LSP time to index
    std::thread::sleep(Duration::from_millis(500));

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

    // bash-language-server may or may not support workspace symbols well enough
    // for this to work, so we accept either success with results or a "not found" message
    let content_arr = result["content"]
        .as_array()
        .context("Missing content array")?;
    if result["isError"] == true {
        let text = content_arr[0]["text"]
            .as_str()
            .context("Missing text in content")?;
        // Accept "Symbol not found" as a valid response for bash-lsp
        assert!(
            text.contains("not found") || text.contains("No references"),
            "Unexpected error: {text}"
        );
    } else if !content_arr.is_empty() {
        let text = content_arr[0]["text"]
            .as_str()
            .context("Missing text in content")?;
        // If we got results, they should contain the file path
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
    require_bash_lsp!();

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp")?;
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
fn test_multi_root_hover() -> Result<()> {
    require_bash_lsp!();

    // Create two separate workspace roots with different scripts
    let dir_a = tempfile::tempdir().context("Failed to create temp dir A")?;
    let dir_b = tempfile::tempdir().context("Failed to create temp dir B")?;

    let script_a = dir_a.path().join("script_a.sh");
    std::fs::write(&script_a, "#!/bin/bash\necho \"from root A\"\n")?;

    let script_b = dir_b.path().join("script_b.sh");
    std::fs::write(&script_b, "#!/bin/bash\necho \"from root B\"\n")?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    let mut bridge = BridgeProcess::spawn_multi_root(
        &["shellscript:bash-language-server start"],
        &[root_a, root_b],
    )?;
    bridge.initialize()?;

    // Hover on echo in script from root A
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 600,
        "method": "tools/call",
        "params": {
            "name": "hover",
            "arguments": {
                "file": script_a.to_str().context("Invalid script A path")?,
                "line": 1,
                "character": 0
            }
        }
    }))?;

    let response_a = bridge.recv()?;
    let result_a = &response_a["result"];
    assert!(
        result_a["isError"].is_null() || result_a["isError"] == false,
        "Hover on root A script failed: {response_a:?}"
    );
    let content_a = result_a["content"]
        .as_array()
        .context("Missing content array for root A")?;
    assert!(!content_a.is_empty(), "Expected hover content for root A");
    let text_a = content_a[0]["text"]
        .as_str()
        .context("Missing text in root A content")?;
    assert!(
        text_a.contains("echo"),
        "Hover should contain 'echo' for root A, got: {text_a}"
    );

    // Hover on echo in script from root B
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 601,
        "method": "tools/call",
        "params": {
            "name": "hover",
            "arguments": {
                "file": script_b.to_str().context("Invalid script B path")?,
                "line": 1,
                "character": 0
            }
        }
    }))?;

    let response_b = bridge.recv()?;
    let result_b = &response_b["result"];
    assert!(
        result_b["isError"].is_null() || result_b["isError"] == false,
        "Hover on root B script failed: {response_b:?}"
    );
    let content_b = result_b["content"]
        .as_array()
        .context("Missing content array for root B")?;
    assert!(!content_b.is_empty(), "Expected hover content for root B");
    let text_b = content_b[0]["text"]
        .as_str()
        .context("Missing text in root B content")?;
    assert!(
        text_b.contains("echo"),
        "Hover should contain 'echo' for root B, got: {text_b}"
    );

    Ok(())
}

#[test]
fn test_multi_root_find_symbol() -> Result<()> {
    require_bash_lsp!();

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

    let mut bridge = BridgeProcess::spawn_multi_root(
        &["shellscript:bash-language-server start"],
        &[root_a, root_b],
    )?;
    bridge.initialize()?;

    // Give LSP time to index
    std::thread::sleep(Duration::from_secs(2));

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
    require_bash_lsp!();

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

    let mut bridge = BridgeProcess::spawn_multi_root(
        &["shellscript:bash-language-server start"],
        &[root_a, root_b],
    )?;
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
    require_bash_lsp!();

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

    let mut bridge = BridgeProcess::spawn_multi_root(
        &["shellscript:bash-language-server start"],
        &[root_a, root_b],
    )?;
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

// ─── roots/list tests ───────────────────────────────────────────────────

#[test]
fn test_roots_list_after_initialize() -> Result<()> {
    require_bash_lsp!();

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp")?;

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
    require_bash_lsp!();

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp")?;

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
    require_bash_lsp!();

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp")?;

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

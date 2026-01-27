//! End-to-end integration tests for the MCP-LSP bridge.
//!
//! These tests spawn the actual bridge binary and communicate with it
//! via stdin/stdout using the MCP protocol.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

/// Check if a command exists in PATH
fn command_exists(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

macro_rules! require_bash_lsp {
    () => {
        if !command_exists("bash-language-server") {
            eprintln!("Skipping test: bash-language-server not installed");
            return;
        }
    };
}

/// Helper to spawn the bridge and communicate with it
struct BridgeProcess {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl BridgeProcess {
    fn spawn(lsp_command: &str, root: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_catenary"))
            .args(["--command", lsp_command, "--root", root])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("Failed to spawn bridge");

        let stdin = child.stdin.take().expect("Failed to get stdin");
        let stdout = BufReader::new(child.stdout.take().expect("Failed to get stdout"));

        // Give LSP server time to initialize
        std::thread::sleep(Duration::from_millis(500));

        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, request: &Value) {
        let json = serde_json::to_string(request).unwrap();
        writeln!(self.stdin, "{}", json).expect("Failed to write to stdin");
        self.stdin.flush().expect("Failed to flush stdin");
    }

    fn recv(&mut self) -> Value {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("Failed to read from stdout");
        serde_json::from_str(&line).expect("Failed to parse JSON response")
    }

    fn initialize(&mut self) {
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
        }));

        let response = self.recv();
        assert!(
            response.get("result").is_some(),
            "Initialize failed: {:?}",
            response
        );

        // Send initialized notification
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }));

        // Small delay for notification processing
        std::thread::sleep(Duration::from_millis(100));
    }
}

impl Drop for BridgeProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[test]
fn test_mcp_initialize() {
    require_bash_lsp!();

    let mut bridge = BridgeProcess::spawn("bash-language-server start", "/tmp");

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
    }));

    let response = bridge.recv();

    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert!(response.get("result").is_some());

    let result = &response["result"];
    assert_eq!(result["protocolVersion"], "2024-11-05");
    assert_eq!(result["serverInfo"]["name"], "catenary");
    assert!(result["capabilities"]["tools"].is_object());
}

#[test]
fn test_mcp_tools_list() {
    require_bash_lsp!();

    let mut bridge = BridgeProcess::spawn("bash-language-server start", "/tmp");
    bridge.initialize();

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list"
    }));

    let response = bridge.recv();

    assert!(response.get("result").is_some());
    let tools = response["result"]["tools"].as_array().unwrap();

    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    // Check all expected tools are present
    let expected_tools = [
        "lsp_hover",
        "lsp_definition",
        "lsp_type_definition",
        "lsp_implementation",
        "lsp_references",
        "lsp_document_symbols",
        "lsp_workspace_symbols",
        "lsp_code_actions",
        "lsp_rename",
        "lsp_completion",
        "lsp_diagnostics",
        "lsp_signature_help",
        "lsp_formatting",
        "lsp_range_formatting",
        "lsp_call_hierarchy",
        "lsp_type_hierarchy",
    ];

    for expected in &expected_tools {
        assert!(tool_names.contains(expected), "Missing {} tool", expected);
    }

    // Verify all tools have valid schemas
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        assert!(
            tool.get("inputSchema").is_some(),
            "Tool {} missing inputSchema",
            name
        );
        let schema = &tool["inputSchema"];
        assert_eq!(
            schema["type"], "object",
            "Tool {} schema type is not object",
            name
        );
        assert!(
            schema["properties"].is_object(),
            "Tool {} has no properties",
            name
        );
    }
}

#[test]
fn test_mcp_hover_builtin() {
    require_bash_lsp!();

    // Create a test script
    let test_file = "/tmp/mcp_test_hover.sh";
    std::fs::write(test_file, "#!/bin/bash\necho \"hello\"\n").unwrap();

    let mut bridge = BridgeProcess::spawn("bash-language-server start", "/tmp");
    bridge.initialize();

    // Request hover on 'echo' (line 1, character 0)
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "lsp_hover",
            "arguments": {
                "file": test_file,
                "line": 1,
                "character": 0
            }
        }
    }));

    let response = bridge.recv();

    assert!(
        response.get("result").is_some(),
        "Hover call failed: {:?}",
        response
    );

    let result = &response["result"];
    assert!(result["isError"].is_null() || result["isError"] == false);

    let content = result["content"].as_array().unwrap();
    assert!(!content.is_empty(), "Expected hover content");

    let text = content[0]["text"].as_str().unwrap();
    assert!(
        text.contains("echo"),
        "Hover should contain 'echo' documentation, got: {}",
        text
    );

    std::fs::remove_file(test_file).ok();
}

#[test]
fn test_mcp_definition() {
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
    )
    .unwrap();

    let mut bridge = BridgeProcess::spawn("bash-language-server start", "/tmp");
    bridge.initialize();

    // Request definition on 'my_function' call (line 6, character 0)
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "lsp_definition",
            "arguments": {
                "file": test_file,
                "line": 6,
                "character": 0
            }
        }
    }));

    let response = bridge.recv();

    assert!(
        response.get("result").is_some(),
        "Definition call failed: {:?}",
        response
    );

    let result = &response["result"];
    assert!(result["isError"].is_null() || result["isError"] == false);

    let content = result["content"].as_array().unwrap();
    assert!(!content.is_empty(), "Expected definition content");

    let text = content[0]["text"].as_str().unwrap();
    // Should point to line 3 where my_function is defined
    assert!(
        text.contains(test_file) && text.contains(":3:"),
        "Definition should point to function definition at line 3, got: {}",
        text
    );

    std::fs::remove_file(test_file).ok();
}

#[test]
fn test_mcp_hover_no_info() {
    require_bash_lsp!();

    // Create a test script
    let test_file = "/tmp/mcp_test_hover_empty.sh";
    std::fs::write(test_file, "#!/bin/bash\n# just a comment\n").unwrap();

    let mut bridge = BridgeProcess::spawn("bash-language-server start", "/tmp");
    bridge.initialize();

    // Request hover on comment (line 1, character 0)
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "lsp_hover",
            "arguments": {
                "file": test_file,
                "line": 1,
                "character": 2
            }
        }
    }));

    let response = bridge.recv();

    assert!(response.get("result").is_some());

    let result = &response["result"];
    // Should not be an error, just no hover info
    assert!(result["isError"].is_null() || result["isError"] == false);

    std::fs::remove_file(test_file).ok();
}

#[test]
fn test_mcp_tool_call_invalid_file() {
    require_bash_lsp!();

    let mut bridge = BridgeProcess::spawn("bash-language-server start", "/tmp");
    bridge.initialize();

    // Request hover on non-existent file
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "name": "lsp_hover",
            "arguments": {
                "file": "/tmp/nonexistent_file_12345.sh",
                "line": 0,
                "character": 0
            }
        }
    }));

    let response = bridge.recv();

    assert!(response.get("result").is_some());

    let result = &response["result"];
    // Should return an error result
    assert_eq!(
        result["isError"], true,
        "Expected error for nonexistent file"
    );
}

#[test]
fn test_mcp_tool_call_unknown_tool() {
    require_bash_lsp!();

    let mut bridge = BridgeProcess::spawn("bash-language-server start", "/tmp");
    bridge.initialize();

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "unknown_tool",
            "arguments": {}
        }
    }));

    let response = bridge.recv();

    assert!(response.get("result").is_some());

    let result = &response["result"];
    assert_eq!(result["isError"], true, "Expected error for unknown tool");
}

#[test]
fn test_mcp_ping() {
    require_bash_lsp!();

    let mut bridge = BridgeProcess::spawn("bash-language-server start", "/tmp");
    bridge.initialize();

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "ping"
    }));

    let response = bridge.recv();

    assert!(response.get("result").is_some());
    assert!(response.get("error").is_none());
}

#[test]
fn test_mcp_diagnostics() {
    require_bash_lsp!();

    // Create a bash script - diagnostics may or may not find issues
    let test_file = "/tmp/mcp_test_diagnostics.sh";
    std::fs::write(test_file, "#!/bin/bash\necho $undefined_var\n").unwrap();

    let mut bridge = BridgeProcess::spawn("bash-language-server start", "/tmp");
    bridge.initialize();

    // Request diagnostics
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/call",
        "params": {
            "name": "lsp_diagnostics",
            "arguments": {
                "file": test_file
            }
        }
    }));

    let response = bridge.recv();

    assert!(
        response.get("result").is_some(),
        "Diagnostics call failed: {:?}",
        response
    );

    let result = &response["result"];
    assert!(result["isError"].is_null() || result["isError"] == false);

    let content = result["content"].as_array().unwrap();
    assert!(!content.is_empty(), "Expected diagnostic content");

    // Should have text content (either "No diagnostics" or actual diagnostics)
    let text = content[0]["text"].as_str().unwrap();
    assert!(!text.is_empty(), "Expected non-empty diagnostic text");

    std::fs::remove_file(test_file).ok();
}

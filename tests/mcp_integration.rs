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

/// Find line and column (0-indexed) of a substring
fn find_position(content: &str, substring: &str) -> (u32, u32) {
    for (line_idx, line) in content.lines().enumerate() {
        if let Some(col_idx) = line.find(substring) {
            return (line_idx as u32, col_idx as u32);
        }
    }
    panic!("Substring '{}' not found in content", substring);
}

macro_rules! require_bash_lsp {
    () => {
        if !command_exists("bash-language-server") {
            eprintln!("Skipping test: bash-language-server not installed");
            return;
        }
    };
}

macro_rules! require_taplo {
    () => {
        if !command_exists("taplo") {
            eprintln!("Skipping test: taplo not installed");
            return;
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
    fn spawn(lsp_commands: &[&str], root: &str) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));

        for lsp in lsp_commands {
            cmd.arg("--lsp").arg(lsp);
        }

        cmd.arg("--root").arg(root);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn().expect("Failed to spawn bridge");

        let stdin = child.stdin.take().expect("Failed to get stdin");
        let stdout = BufReader::new(child.stdout.take().expect("Failed to get stdout"));

        // Give LSP server time to initialize
        std::thread::sleep(Duration::from_millis(500));

        Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
        }
    }

    fn send(&mut self, request: &Value) {
        let json = serde_json::to_string(request).unwrap();
        let stdin = self.stdin.as_mut().expect("Stdin already closed");
        writeln!(stdin, "{}", json).expect("Failed to write to stdin");
        stdin.flush().expect("Failed to flush stdin");
    }

    fn recv(&mut self) -> Value {
        let mut line = String::new();
        let stdout = self.stdout.as_mut().expect("Stdout already closed");
        stdout
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
fn test_mcp_initialize() {
    require_bash_lsp!();

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp");

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

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp");
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
        "catenary_find_references",
        "lsp_document_symbols",
        "catenary_find_symbol",
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

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp");
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

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp");
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

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp");
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

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp");
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

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp");
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

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp");
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
fn test_multiplexing() {
    require_bash_lsp!();
    require_taplo!();
    // We assume rust-analyzer is present if we are developing this

    let root_dir = std::env::current_dir().unwrap();
    let root_str = root_dir.to_str().unwrap();
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
    );

    bridge.initialize();

    // 1. Test Rust Hover
    // Find a stable token in src/main.rs, e.g., "println" or "main"
    // Let's use "main" on the first few lines
    let content = std::fs::read_to_string(&rust_file).unwrap();
    let (line, col) = find_position(&content, "fn main");

    // Retry loop for server startup and indexing
    let mut success = false;
    for i in 0..20 {
        // Retry for up to 10 seconds (20 * 500ms)
        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": 100 + i,
            "method": "tools/call",
            "params": {
                "name": "lsp_hover",
                "arguments": {
                    "file": rust_file.to_str().unwrap(),
                    "line": line,
                    "character": col + 3 // hover on 'main'
                }
            }
        }));

        let response = bridge.recv();
        let result = &response["result"];

        if result["isError"] == true {
            let content = result["content"].as_array().unwrap();
            let text = content[0]["text"].as_str().unwrap();
            if text.contains("content modified") || text.contains("No hover information") {
                // Ignore errors during warm-up
            } else {
                // Genuine error
                eprintln!("Unexpected error: {}", text);
            }
        } else {
            let content = result["content"].as_array().unwrap();
            let text = content[0]["text"].as_str().unwrap();
            if text.contains("No hover information") {
                // Not ready yet
            } else if text.contains("main") {
                success = true;
                break;
            }
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
            "name": "lsp_hover",
            "arguments": {
                "file": bash_file.to_str().unwrap(),
                "line": 2, // echo line
                "character": 4 // echo command
            }
        }
    }));

    let response = bridge.recv();
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Bash hover failed: {:?}",
        response
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
                "name": "lsp_hover",
                "arguments": {
                    "file": toml_file.to_str().unwrap(),
                    "line": 1, // name = ...
                    "character": 0 // name
                }
            }
        }));

        let response = bridge.recv();
        let result = &response["result"];

        if result["isError"] == true {
            let content = result["content"].as_array().unwrap();
            let text = content[0]["text"].as_str().unwrap();
            // Taplo might timeout on first request while spawning
            if text.contains("timed out") {
                // retry
            } else {
                eprintln!("Unexpected TOML error: {}", text);
            }
        } else {
            let content = result["content"].as_array().unwrap();
            let text = content[0]["text"].as_str().unwrap();
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
            "name": "catenary_find_symbol",
            "arguments": {
                "query": "greet"
            }
        }
    }));

    let response = bridge.recv();
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Find symbol failed"
    );
    let text = result["content"][0]["text"].as_str().unwrap();

    assert!(text.contains("greet"), "Expected to find 'greet' symbol");
}

#[test]
fn test_client_info_stored_in_session() {
    require_bash_lsp!();

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp");

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
    }));

    let response = bridge.recv();
    assert!(response.get("result").is_some(), "Initialize failed");

    // Small delay to allow session update
    std::thread::sleep(Duration::from_millis(200));

    // Run catenary list and check output
    let output = Command::new(env!("CARGO_BIN_EXE_catenary"))
        .arg("list")
        .output()
        .expect("Failed to run catenary list");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Note: The client name may be truncated in the list output, so we only check for TestClient
    assert!(
        stdout.contains("TestClient"),
        "Expected client info 'TestClient' in catenary list output, got:\n{}",
        stdout
    );
}

#[test]
fn test_catenary_find_references_by_position() {
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
    )
    .unwrap();

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp");
    bridge.initialize();

    // Request references by position (on the function definition, line 2)
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 500,
        "method": "tools/call",
        "params": {
            "name": "catenary_find_references",
            "arguments": {
                "file": test_file,
                "line": 2,
                "character": 0
            }
        }
    }));

    let response = bridge.recv();

    assert!(
        response.get("result").is_some(),
        "Find references call failed: {:?}",
        response
    );

    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Expected success, got error: {:?}",
        result
    );

    let content = result["content"].as_array().unwrap();
    let text = content[0]["text"].as_str().unwrap();

    // Should find at least the definition and calls
    // The definition should be marked with [def]
    assert!(
        text.contains("[def]"),
        "Expected definition marker [def] in output, got: {}",
        text
    );

    std::fs::remove_file(test_file).ok();
}

#[test]
fn test_catenary_find_references_by_symbol() {
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
    )
    .unwrap();

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp");
    bridge.initialize();

    // Give LSP time to index
    std::thread::sleep(Duration::from_millis(500));

    // Request references by symbol name
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 501,
        "method": "tools/call",
        "params": {
            "name": "catenary_find_references",
            "arguments": {
                "symbol": "unique_test_func",
                "file": test_file
            }
        }
    }));

    let response = bridge.recv();

    let result = &response["result"];

    // bash-language-server may or may not support workspace symbols well enough
    // for this to work, so we accept either success with results or a "not found" message
    if result["isError"] == true {
        let content = result["content"].as_array().unwrap();
        let text = content[0]["text"].as_str().unwrap();
        // Accept "Symbol not found" as a valid response for bash-lsp
        assert!(
            text.contains("not found") || text.contains("No references"),
            "Unexpected error: {}",
            text
        );
    } else {
        let content = result["content"].as_array().unwrap();
        if !content.is_empty() {
            let text = content[0]["text"].as_str().unwrap();
            // If we got results, they should contain the file path
            assert!(
                text.contains(test_file) || text.contains("No references"),
                "Expected references to contain file path, got: {}",
                text
            );
        }
    }

    std::fs::remove_file(test_file).ok();
}

#[test]
fn test_catenary_find_references_missing_args() {
    require_bash_lsp!();

    let mut bridge = BridgeProcess::spawn(&["shellscript:bash-language-server start"], "/tmp");
    bridge.initialize();

    // Request without symbol or file - should fail
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 502,
        "method": "tools/call",
        "params": {
            "name": "catenary_find_references",
            "arguments": {}
        }
    }));

    let response = bridge.recv();

    let result = &response["result"];
    assert_eq!(
        result["isError"], true,
        "Expected error for missing arguments"
    );
}

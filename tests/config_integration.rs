use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;
use serde_json::{Value, json};

#[test]
fn test_config_loading() {
    let root_dir = std::env::current_dir().unwrap();
    let config_path = root_dir.join("tests/assets/config.toml");
    let bash_file = root_dir.join("tests/assets/bash/script.sh");

    // Spawn catenary using ONLY the config file (no --lsp args)
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("--config").arg(config_path);
    cmd.arg("--root").arg(root_dir); // Catenary root
    
    cmd.stdin(Stdio::piped())
       .stdout(Stdio::piped())
       .stderr(Stdio::inherit()); // See logs

    let mut child = cmd.spawn().expect("Failed to spawn catenary");
    let mut stdin = child.stdin.take().expect("Failed to get stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("Failed to get stdout"));

    // Wait for init
    std::thread::sleep(Duration::from_secs(2));

    // Initialize MCP
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        }
    });
    writeln!(stdin, "{}", init_req).unwrap();
    
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert!(response.get("result").is_some(), "Init failed: {:?}", response);

    // Initialized notification
    writeln!(stdin, "{}", json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    })).unwrap();

    // Test Bash Hover (should be enabled by config)
    writeln!(stdin, "{}", json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "lsp_hover",
            "arguments": {
                "file": bash_file.to_str().unwrap(),
                "line": 2,
                "character": 4
            }
        }
    })).unwrap();

    line.clear();
    stdout.read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    
    // Cleanup - drop stdin to signal EOF, then wait for graceful exit
    drop(stdin);
    let _ = child.wait();

    let result = &response["result"];
    assert!(result["isError"].is_null() || result["isError"] == false, "Bash hover failed: {:?}", response);
}

#[test]
fn test_config_override() {
    let root_dir = std::env::current_dir().unwrap();
    let config_path = root_dir.join("tests/assets/config.toml");
    let rust_file = root_dir.join("src/main.rs");

    // Spawn catenary with config AND CLI override
    // Config provides 'shellscript', CLI provides 'rust'
    // CLI also overrides idle_timeout to 10 (config has 60)
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("--config").arg(config_path);
    cmd.arg("--lsp").arg("rust:rust-analyzer"); 
    cmd.arg("--idle-timeout").arg("10");
    cmd.arg("--root").arg(root_dir);
    
    cmd.stdin(Stdio::piped())
       .stdout(Stdio::piped())
       .stderr(Stdio::inherit());

    let mut child = cmd.spawn().expect("Failed to spawn catenary");
    let mut stdin = child.stdin.take().expect("Failed to get stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("Failed to get stdout"));

    // Wait for init (Rust analyzer needs time)
    std::thread::sleep(Duration::from_secs(3));

    // Initialize MCP
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        }
    });
    writeln!(stdin, "{}", init_req).unwrap();
    
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert!(response.get("result").is_some(), "Init failed");

    // Send initialized
    writeln!(stdin, "{}", json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).unwrap();

    // Test Rust Hover (CLI arg) - should work
    writeln!(stdin, "{}", json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "lsp_hover",
            "arguments": {
                "file": rust_file.to_str().unwrap(),
                "line": 1,
                "character": 0
            }
        }
    })).unwrap();

    line.clear();
    stdout.read_line(&mut line).unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    
    let result = &response["result"];
    assert!(result["isError"].is_null() || result["isError"] == false, "Rust hover failed (CLI arg not merged?)");

    // Cleanup - drop stdin to signal EOF, then wait for graceful exit
    drop(stdin);
    let _ = child.wait();
}

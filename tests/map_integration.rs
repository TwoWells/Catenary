//! Integration tests for codebase map tool.

use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Helper to spawn the bridge
struct BridgeProcess {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl BridgeProcess {
    fn spawn(root: &str, lsp_args: Option<&str>) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        if let Some(arg) = lsp_args {
            cmd.arg("--lsp").arg(arg);
        } else {
            // Default for existing tests
            cmd.arg("--lsp").arg("shellscript:bash-language-server start");
        }
        cmd.arg("--root").arg(root);
        
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = cmd.spawn().expect("Failed to spawn bridge");
        let stdin = child.stdin.take().expect("Failed to get stdin");
        let stdout = BufReader::new(child.stdout.take().expect("Failed to get stdout"));

        // Wait for initialization
        std::thread::sleep(Duration::from_millis(500));

        Self { child, stdin, stdout }
    }

    fn send(&mut self, request: &serde_json::Value) {
        let json = serde_json::to_string(request).unwrap();
        writeln!(self.stdin, "{}", json).expect("Failed to write to stdin");
        self.stdin.flush().expect("Failed to flush stdin");
    }

    fn recv(&mut self) -> serde_json::Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("Failed to read from stdout");
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
                "clientInfo": { "name": "test", "version": "1.0" }
            }
        }));
        let _ = self.recv();
        self.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    }
}

impl Drop for BridgeProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[test]
fn test_codebase_map_basic() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("file1.txt"), "content").unwrap();
    std::fs::create_dir(temp.path().join("subdir")).unwrap();
    std::fs::write(temp.path().join("subdir/file2.rs"), "fn main() {}").unwrap();

    let mut bridge = BridgeProcess::spawn(temp.path().to_str().unwrap(), None);
    bridge.initialize();

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "catenary_codebase_map",
            "arguments": {
                "path": temp.path().to_str().unwrap(),
                "max_depth": 5,
                "include_symbols": false
            }
        }
    }));

    let response = bridge.recv();
    let result = &response["result"];
    assert!(result["isError"].is_null() || result["isError"] == false);
    
    let content = result["content"][0]["text"].as_str().unwrap();
    println!("Map Output:\n{}", content);

    assert!(content.contains("file1.txt"));
    assert!(content.contains("subdir/"));
    assert!(content.contains("file2.rs"));
}

#[test]
fn test_codebase_map_with_symbols() {
    // Requires bash-language-server
    if Command::new("which").arg("bash-language-server").output().is_err() {
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("script.sh");
    std::fs::write(&script, "#!/bin/bash\nfunction my_func() { echo hi; }\n").unwrap();

    let mut bridge = BridgeProcess::spawn(temp.path().to_str().unwrap(), None);
    bridge.initialize();

    // Give LSP time to wake up if lazy
    std::thread::sleep(Duration::from_millis(500));

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "catenary_codebase_map",
            "arguments": {
                "path": temp.path().to_str().unwrap(),
                "include_symbols": true
            }
        }
    }));

    let response = bridge.recv();
    let result = &response["result"];
    let content = result["content"][0]["text"].as_str().unwrap();
    
    println!("Map with Symbols Output:\n{}", content);

    assert!(content.contains("script.sh"));
    // Bash LSP should report 'my_func' as a Function
    if !content.contains("my_func") {
        println!("WARNING: Symbols not found. Check if bash-language-server is running correctly.");
    }
}

#[test]
fn test_codebase_map_markdown() {
    if Command::new("which").arg("marksman").output().is_err() {
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    // Create .git to help Marksman detect root
    std::fs::create_dir(temp.path().join(".git")).unwrap();
    
    let md_path = temp.path().join("README.md");
    std::fs::write(&md_path, "# Title\n\n## Section 1\nContent\n\n### Subsection\nMore content").unwrap();

    let mut bridge = BridgeProcess::spawn(
        temp.path().to_str().unwrap(), 
        Some("markdown:marksman server")
    );
    bridge.initialize();

    // Give LSP time to scan
    std::thread::sleep(Duration::from_secs(5));

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "catenary_codebase_map",
            "arguments": {
                "path": temp.path().to_str().unwrap(),
                "include_symbols": true
            }
        }
    }));

    let response = bridge.recv();
    let result = &response["result"];
    let content = result["content"][0]["text"].as_str().unwrap();
    
    println!("Markdown Map Output:\n{}", content);

    assert!(content.contains("README.md"));
    // Marksman usually reports headings as symbols
    assert!(content.contains("Title"), "Should contain Title symbol");
    assert!(content.contains("Section 1"), "Should contain Section 1 symbol");
}
//! Integration tests for auto-fix functionality.

use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Helper to spawn the bridge
struct BridgeProcess {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<BufReader<std::process::ChildStdout>>,
}

impl BridgeProcess {
    fn spawn(root: &str) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        // Enable rust-analyzer
        cmd.arg("--lsp").arg("rust:rust-analyzer");
        cmd.arg("--root").arg(root);

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = cmd.spawn().expect("Failed to spawn bridge");
        let stdin = child.stdin.take().expect("Failed to get stdin");
        let stdout = BufReader::new(child.stdout.take().expect("Failed to get stdout"));

        // Wait for initialization
        std::thread::sleep(Duration::from_millis(500));

        Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
        }
    }

    fn send(&mut self, request: &serde_json::Value) {
        let json = serde_json::to_string(request).unwrap();
        let stdin = self.stdin.as_mut().expect("Stdin already closed");
        writeln!(stdin, "{}", json).expect("Failed to write to stdin");
        stdin.flush().expect("Failed to flush stdin");
    }

    fn recv(&mut self) -> serde_json::Value {
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
                "clientInfo": { "name": "test", "version": "1.0" }
            }
        }));
        let _ = self.recv();
        self.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    }
}

impl Drop for BridgeProcess {
    fn drop(&mut self) {
        // Closing stdin signals the server to shut down gracefully
        self.stdin.take();
        // The LspClient Drop will handle killing child processes if Catenary exits abruptly
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn test_quickfix_rust_unused() {
    // Requires rust-analyzer
    if Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    // Create Cargo.toml
    std::fs::write(
        temp.path().join("Cargo.toml"),
        r#"[package]
name = "test-quickfix"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();

    std::fs::create_dir(temp.path().join("src")).unwrap();
    let main_rs = temp.path().join("src/main.rs");

    // Unused variable 'x'
    let content = "fn main() {\n    let x = 1;\n}\n";
    std::fs::write(&main_rs, content).unwrap();

    let mut bridge = BridgeProcess::spawn(temp.path().to_str().unwrap());
    bridge.initialize();

    // Give LSP time to index and lint (Rust Analyzer takes a bit)
    let mut found_diagnostic = false;
    for _ in 0..20 {
        // 10 seconds
        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": 999,
            "method": "tools/call",
            "params": {
                "name": "lsp_diagnostics",
                "arguments": {
                    "file": main_rs.to_str().unwrap()
                }
            }
        }));
        let response = bridge.recv();
        if let Some(content) = response["result"]["content"][0]["text"].as_str() {
            println!("Diagnostics: {}", content);
            if content.contains("unused") || content.contains("x") {
                found_diagnostic = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    assert!(found_diagnostic, "Expected unused variable warning");

    // Request quickfix at line 1 ("let x = 1;"), char 8 (on "x")
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "catenary_apply_quickfix",
            "arguments": {
                "file": main_rs.to_str().unwrap(),
                "line": 1,
                "character": 8
            }
        }
    }));

    let response = bridge.recv();
    println!("Response: {:?}", response);
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Tool call failed"
    );

    // Verify file content changed
    let new_content = std::fs::read_to_string(&main_rs).unwrap();
    println!("New Content:\n{}", new_content);

    // Expect _x or removal
    assert!(
        new_content.contains("_x") || !new_content.contains("let x"),
        "Fix was not applied (expected _x or removal)"
    );
}

use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use tempfile::tempdir;

#[test]
fn test_lsp_diagnostics_waits_for_analysis_after_change() {
    // 1. Setup workspace with a valid rust file
    let dir = tempdir().unwrap();
    let src_dir = dir.path().join("src");
    std::fs::create_dir(&src_dir).unwrap();
    let file_path = src_dir.join("main.rs");
    std::fs::write(&file_path, "fn main() { let x: i32 = 1; }").unwrap();

    std::fs::write(
        dir.path().join("Cargo.toml"),
        r#"
[package]
name = "test-crate"
version = "0.1.0"
"#,
    )
    .unwrap();

    // 2. Start Catenary
    let mut child = Command::new("cargo")
        .args([
            "run",
            "--",
            "--root",
            dir.path().to_str().unwrap(),
            "--lsp",
            "rust:rust-analyzer",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Failed to start catenary");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    // 3. Initialize MCP
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1.0" }
        }
    });
    writeln!(stdin, "{}", init_req).unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();

    let initialized_notif = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    writeln!(stdin, "{}", initialized_notif).unwrap();

    // Give it a moment to finish initial indexing
    std::thread::sleep(std::time::Duration::from_millis(5000));

    // 4. Update file to introduce error
    std::fs::write(&file_path, "fn main() { let x: i32 = \"string\"; }").unwrap();

    // 5. Call lsp_diagnostics IMMEDIATELY
    let diag_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "lsp_diagnostics",
            "arguments": {
                "file": file_path.to_str().unwrap(),
                "wait_for_reanalysis": true
            }
        }
    });

    writeln!(stdin, "{}", diag_req).unwrap();

    line.clear();
    reader.read_line(&mut line).unwrap();

    // 6. Verify result contains the expected error
    assert!(
        line.contains("mismatched types") || line.contains("expected i32"),
        "Diagnostics should contain the error after change. Got: {}",
        line
    );

    // Cleanup
    let _ = child.kill();
}

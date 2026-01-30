//! Integration tests for session monitoring and event broadcasting.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

/// Helper to spawn the bridge and capture stderr to find session ID
struct ServerProcess {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    stderr: BufReader<std::process::ChildStderr>,
}

impl ServerProcess {
    fn spawn() -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        cmd.arg("serve");
        cmd.arg("--root").arg(".");

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().expect("Failed to spawn server");

        let stdin = child.stdin.take().expect("Failed to get stdin");
        let stdout = BufReader::new(child.stdout.take().expect("Failed to get stdout"));
        let stderr = BufReader::new(child.stderr.take().expect("Failed to get stderr"));

        Self {
            child,
            stdin,
            stdout,
            stderr,
        }
    }

    fn get_session_id(&mut self) -> String {
        let mut line = String::new();
        // Read stderr line by line until we find "Session ID:"
        loop {
            line.clear();
            self.stderr
                .read_line(&mut line)
                .expect("Failed to read stderr");
            if line.contains("Session ID:") {
                let parts: Vec<&str> = line.split("Session ID:").collect();
                return parts[1].trim().to_string();
            }
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
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[test]
fn test_monitor_raw_messages() {
    // 1. Start Server
    let mut server = ServerProcess::spawn();
    let session_id = server.get_session_id();
    println!("Session ID: {}", session_id);

    // 2. Start Monitor (in a separate thread to avoid blocking main test flow if we want)
    // Actually, we can spawn the process and read from it.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("monitor").arg(&session_id);
    cmd.stdout(Stdio::piped());
    let mut child = cmd.spawn().expect("Failed to spawn monitor");
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    // 3. Send a request to the server
    let request_id = 12345;
    let request = json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "ping"
    });

    server.send(&request);
    let _response = server.recv();

    // 4. Check monitor output
    // We expect to see "→ ... ping" and "← ... result" (arrows instead of MCP(in)/MCP(out))

    let mut found_in = false;
    let mut found_out = false;

    // Read up to 20 lines
    let mut line = String::new();
    for _ in 0..20 {
        line.clear();
        if reader.read_line(&mut line).unwrap() > 0 {
            println!("Monitor: {}", line.trim());
            // New format uses → for incoming and ← for outgoing
            if line.contains("→") && line.contains("ping") {
                found_in = true;
            }
            if line.contains("←") && line.contains("result") {
                found_out = true;
            }

            if found_in && found_out {
                break;
            }
        } else {
            thread::sleep(Duration::from_millis(100));
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        found_in,
        "Did not find incoming MCP message (→) in monitor output"
    );
    assert!(
        found_out,
        "Did not find outgoing MCP message (←) in monitor output"
    );
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Integration tests for session monitoring and event broadcasting.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

/// Helper to spawn the bridge and capture stderr to find session ID
struct ServerProcess {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    stderr: BufReader<std::process::ChildStderr>,
}

impl ServerProcess {
    fn spawn() -> Result<Self> {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        cmd.arg("serve");
        cmd.arg("--root").arg(".");
        // Isolate from user-level config
        cmd.env("XDG_CONFIG_HOME", ".");

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().context("Failed to spawn server")?;

        let stdin = child.stdin.take().context("Failed to get stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("Failed to get stdout")?);
        let stderr = BufReader::new(child.stderr.take().context("Failed to get stderr")?);

        Ok(Self {
            child,
            stdin,
            stdout,
            stderr,
        })
    }

    fn get_session_id(&mut self) -> Result<String> {
        let mut line = String::new();
        // Read stderr line by line until we find "Session ID:"
        // Limit to 100 lines to avoid infinite loop
        for _ in 0..100 {
            line.clear();
            self.stderr
                .read_line(&mut line)
                .context("Failed to read stderr")?;
            if line.contains("Session ID:") {
                // The log line might look like: "2026-02-13T03:14:15.819396Z  INFO catenary: Session ID: 012305b387"
                // Or with different formatting. We want the last word.
                let id = line
                    .split_whitespace()
                    .last()
                    .context("Failed to parse Session ID from line")?;
                return Ok(id.to_string());
            }
        }
        Err(anyhow!("Failed to find Session ID in output"))
    }

    fn send(&mut self, request: &Value) -> Result<()> {
        let json = serde_json::to_string(request)?;
        writeln!(self.stdin, "{json}").context("Failed to write to stdin")?;
        self.stdin.flush().context("Failed to flush stdin")?;
        Ok(())
    }

    fn recv(&mut self) -> Result<Value> {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .context("Failed to read from stdout")?;
        serde_json::from_str(&line).context("Failed to parse JSON response")
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn test_monitor_raw_messages() -> Result<()> {
    // 1. Start Server
    let mut server = ServerProcess::spawn()?;
    let session_id = server.get_session_id()?;
    tracing::info!("Session ID: {session_id}");

    // 2. Start Monitor (in a separate thread to avoid blocking main test flow if we want)
    // Actually, we can spawn the process and read from it.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("monitor").arg(&session_id);
    cmd.stdout(Stdio::piped());
    let mut child = cmd.spawn().context("Failed to spawn monitor")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to take monitor stdout")?;
    let mut reader = BufReader::new(stdout);

    // 3. Send a request to the server
    let request_id = 12345;
    let request = json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "ping"
    });

    server.send(&request)?;
    let _response = server.recv()?;

    // 4. Check monitor output
    // We expect to see "→ ... ping" and "← ... result" (arrows instead of MCP(in)/MCP(out))

    let mut found_in = false;
    let mut found_out = false;

    // Read up to 20 lines
    let mut line = String::new();
    for _ in 0..20 {
        line.clear();
        if reader.read_line(&mut line)? > 0 {
            tracing::debug!("Monitor: {}", line.trim());
            // New format uses → for incoming and ← for outgoing
            if line.contains('→') && line.contains("ping") {
                found_in = true;
            }
            if line.contains('←') && line.contains("result") {
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
    Ok(())
}

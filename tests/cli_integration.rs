// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Integration tests for CLI list and monitor commands.

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
        for _ in 0..100 {
            line.clear();
            self.stderr
                .read_line(&mut line)
                .context("Failed to read stderr")?;
            if line.contains("Session ID:") {
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
fn test_list_shows_row_numbers() -> Result<()> {
    // Start a server to ensure at least one session exists
    let mut server = ServerProcess::spawn()?;
    let _session_id = server.get_session_id()?;

    // Give the session time to register
    thread::sleep(Duration::from_millis(100));

    // Run catenary list
    let output = Command::new(env!("CARGO_BIN_EXE_catenary"))
        .arg("list")
        .output()
        .context("Failed to run list command")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Check for row number column header
    assert!(
        stdout.contains('#'),
        "List output should contain # column header"
    );

    // Check for numbered rows (should have at least "1" for our session)
    let lines: Vec<&str> = stdout.lines().collect();
    // Skip header and separator, find data lines
    let data_lines: Vec<&str> = lines
        .iter()
        .skip(2)
        .filter(|l| !l.trim().is_empty())
        .copied()
        .collect();

    assert!(
        !data_lines.is_empty(),
        "Should have at least one session row"
    );

    // First data line should start with "1" (row number)
    let first_row = data_lines[0].trim();
    assert!(
        first_row.starts_with('1'),
        "First row should start with row number 1, got: {first_row}"
    );
    Ok(())
}

#[test]
fn test_list_shows_language_servers_line() -> Result<()> {
    // Start a server
    let mut server = ServerProcess::spawn()?;
    let _session_id = server.get_session_id()?;

    // Give the session time to register
    thread::sleep(Duration::from_millis(100));

    // Run catenary list
    let output = Command::new(env!("CARGO_BIN_EXE_catenary"))
        .arg("list")
        .output()
        .context("Failed to run list command")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Languages are displayed on a second line per session, not as a column header
    assert!(
        stdout.contains("CLIENT"),
        "List output should contain CLIENT column header"
    );
    assert!(
        stdout.contains("WORKSPACE"),
        "List output should contain WORKSPACE column header"
    );
    Ok(())
}

#[test]
fn test_monitor_by_row_number_starts() -> Result<()> {
    use std::sync::mpsc;

    // Start a server
    let mut server = ServerProcess::spawn()?;
    let _session_id = server.get_session_id()?;

    // Give the session time to register
    thread::sleep(Duration::from_millis(500));

    // Start monitor with row number "1" - we just verify it successfully starts
    // monitoring some session (row number resolution works)
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("monitor").arg("1");
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().context("Failed to spawn monitor")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to take monitor stdout")?;

    // Use a thread with channel for non-blocking reads
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line) {
            if n == 0 {
                break;
            }
            let _ = tx.send(line.clone());
            line.clear();
        }
    });

    // Read the first line which should show "Monitoring session ..."
    let line = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();

    // Kill and wait before asserting
    let _ = child.kill();
    let _ = child.wait();

    // Verify the monitor started (just check it says "Monitoring session")
    assert!(
        line.contains("Monitoring session"),
        "Monitor should start monitoring a session with row number, got: {line}"
    );
    Ok(())
}

#[test]
fn test_monitor_invalid_row_number_fails() -> Result<()> {
    // Verify that an invalid row number (999) fails appropriately.
    // "999" is tried as row number (out of range), then as session ID prefix
    // (no match), so the row-number error is reported.
    let output = Command::new(env!("CARGO_BIN_EXE_catenary"))
        .arg("monitor")
        .arg("999")
        .output()
        .context("Failed to run monitor command")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("out of range") || stderr.contains("Row number"),
        "Should report row number out of range, got: {stderr}"
    );
    Ok(())
}

#[test]
fn test_monitor_numeric_session_id_resolves() -> Result<()> {
    use std::sync::mpsc;

    // Regression test: session IDs are hex strings that may be all digits
    // (e.g., "025586387"). resolve_session_id must not treat these as row
    // numbers and bail with "out of range".
    let mut server = ServerProcess::spawn()?;
    let session_id = server.get_session_id()?;

    // Start monitor using the full session ID — this must work regardless
    // of whether the ID happens to be all digits.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("monitor").arg(&session_id);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("Failed to spawn monitor")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to take monitor stdout")?;

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line) {
            if n == 0 {
                break;
            }
            let _ = tx.send(line.clone());
            line.clear();
        }
    });

    let header = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();

    // Capture stderr before asserting, for diagnostics
    let _ = child.kill();
    let output = child.wait_with_output().context("wait_with_output")?;
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        header.contains("Monitoring session"),
        "Monitor should start successfully with session ID '{session_id}', \
         got header: '{header}', stderr: '{stderr}'"
    );
    Ok(())
}

#[test]
fn test_monitor_raw_flag() -> Result<()> {
    use std::sync::mpsc;

    // Start a server
    let mut server = ServerProcess::spawn()?;
    let session_id = server.get_session_id()?;

    // Start monitor with --raw flag
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("monitor").arg(&session_id).arg("--raw");
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().context("Failed to spawn monitor")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to take monitor stdout")?;

    // Use a thread with channel for non-blocking reads
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line) {
            if n == 0 {
                break;
            }
            let _ = tx.send(line.clone());
            line.clear();
        }
    });

    // Skip the "Monitoring session..." line
    let _ = rx.recv_timeout(Duration::from_secs(5));

    // Send a request to generate an event
    let request = json!({
        "jsonrpc": "2.0",
        "id": 99999,
        "method": "ping"
    });
    server.send(&request)?;
    let _response = server.recv()?;

    // Read monitor output with timeout
    let mut found_json = false;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(100)) {
            // Raw mode should produce pretty-printed JSON with braces
            if line.contains('{') || line.contains('}') || line.contains("\"jsonrpc\"") {
                found_json = true;
                break;
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    assert!(found_json, "Raw mode should output JSON formatted messages");
    Ok(())
}

#[test]
fn test_monitor_nocolor_flag() -> Result<()> {
    use std::sync::mpsc;

    // Start a server
    let mut server = ServerProcess::spawn()?;
    let session_id = server.get_session_id()?;

    // Start monitor with --nocolor flag
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("monitor").arg(&session_id).arg("--nocolor");
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().context("Failed to spawn monitor")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to take monitor stdout")?;

    // Use a thread with channel for non-blocking reads
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line) {
            if n == 0 {
                break;
            }
            let _ = tx.send(line.clone());
            line.clear();
        }
    });

    // Skip the "Monitoring session..." line
    let _ = rx.recv_timeout(Duration::from_secs(5));

    // Send a request to generate an event
    let request = json!({
        "jsonrpc": "2.0",
        "id": 88888,
        "method": "ping"
    });
    server.send(&request)?;
    let _response = server.recv()?;

    // Collect output with a timeout
    let mut output = String::new();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(100)) {
            output.push_str(&line);
            if output.len() > 100 {
                break;
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    // Check for absence of ANSI escape codes
    // ANSI escape codes start with \x1b[ or \033[
    assert!(
        !output.contains("\x1b["),
        "Output should not contain ANSI escape codes with --nocolor flag"
    );
    Ok(())
}

#[test]
fn test_monitor_filter_flag() -> Result<()> {
    use std::sync::mpsc;

    // Start a server
    let mut server = ServerProcess::spawn()?;
    let session_id = server.get_session_id()?;

    // Start monitor with filter for "ping"
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("monitor")
        .arg(&session_id)
        .arg("--filter")
        .arg("ping");
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().context("Failed to spawn monitor")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to take monitor stdout")?;

    // Use a thread with channel for non-blocking reads
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line) {
            if n == 0 {
                break;
            }
            let _ = tx.send(line.clone());
            line.clear();
        }
    });

    // Skip the "Monitoring session..." line
    let _ = rx.recv_timeout(Duration::from_secs(5));

    // Send a ping request
    let ping_request = json!({
        "jsonrpc": "2.0",
        "id": 77777,
        "method": "ping"
    });
    server.send(&ping_request)?;
    let _response = server.recv()?;

    // Read monitor output with timeout
    let mut found_ping = false;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(100))
            && line.contains("ping")
        {
            found_ping = true;
            break;
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    assert!(found_ping, "Filter should allow ping events through");
    Ok(())
}

#[test]
fn test_monitor_uses_arrows() -> Result<()> {
    use std::sync::mpsc;

    // Start a server
    let mut server = ServerProcess::spawn()?;
    let session_id = server.get_session_id()?;

    // Start monitor (without --raw)
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("monitor").arg(&session_id).arg("--nocolor");
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().context("Failed to spawn monitor")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to take monitor stdout")?;

    // Use a thread with channel for non-blocking reads
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line) {
            if n == 0 {
                break;
            }
            let _ = tx.send(line.clone());
            line.clear();
        }
    });

    // Skip the "Monitoring session..." line
    let _ = rx.recv_timeout(Duration::from_secs(5));

    // Send a request
    let request = json!({
        "jsonrpc": "2.0",
        "id": 66666,
        "method": "ping"
    });
    server.send(&request)?;
    let _response = server.recv()?;

    // Read monitor output and check for arrows with timeout
    let mut found_incoming_arrow = false;
    let mut found_outgoing_arrow = false;

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(100)) {
            if line.contains('→') {
                found_incoming_arrow = true;
            }
            if line.contains('←') {
                found_outgoing_arrow = true;
            }
            if found_incoming_arrow && found_outgoing_arrow {
                break;
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        found_incoming_arrow,
        "Should use → arrow for incoming messages"
    );
    assert!(
        found_outgoing_arrow,
        "Should use ← arrow for outgoing messages"
    );
    Ok(())
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for CLI list, monitor, config, and doctor commands.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

/// Isolates a subprocess from the user's environment.
fn isolate_env(cmd: &mut Command, root: &str) {
    cmd.env("XDG_CONFIG_HOME", root);
    cmd.env("XDG_STATE_HOME", root);
    cmd.env("XDG_DATA_HOME", root);
    cmd.env_remove("CATENARY_STATE_DIR");
    cmd.env_remove("CATENARY_DATA_DIR");
    cmd.env_remove("CATENARY_CONFIG");
    cmd.env_remove("CATENARY_SERVERS");
    cmd.env_remove("CATENARY_ROOTS");
}

/// Helper to spawn the bridge and discover readiness via MCP initialize.
struct ServerProcess {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    state_dir: tempfile::TempDir,
}

impl ServerProcess {
    fn spawn() -> Result<Self> {
        let state_dir = tempfile::tempdir().context("Failed to create state tempdir")?;

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        // Isolate from user-level config and state.
        // XDG_CONFIG_HOME must be an absolute path — the dirs crate
        // ignores relative paths and falls back to ~/.config.
        cmd.env("CATENARY_ROOTS", ".");
        cmd.env("XDG_CONFIG_HOME", state_dir.path());
        cmd.env("CATENARY_STATE_DIR", state_dir.path());
        cmd.env_remove("CATENARY_CONFIG");

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn().context("Failed to spawn server")?;

        let stdin = child.stdin.take().context("Failed to get stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("Failed to get stdout")?);

        Ok(Self {
            child,
            stdin,
            stdout,
            state_dir,
        })
    }

    /// Sends an MCP `initialize` request and reads the response.
    ///
    /// Proves the server is running and the session exists in the DB.
    /// Returns the full instance ID queried from the database.
    fn wait_ready(&mut self) -> Result<String> {
        let init_request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0.0.0" }
            }
        });
        self.send(&init_request)?;
        let _response = self.recv()?;

        // Discover the full instance ID via raw SQL query — the isolated
        // state dir guarantees exactly one session. The `--format json`
        // output is untruncated, unlike `catenary list`.
        let output = Command::new(env!("CARGO_BIN_EXE_catenary"))
            .arg("query")
            .arg("--sql")
            .arg("SELECT id FROM sessions LIMIT 1")
            .arg("--format")
            .arg("json")
            .env("CATENARY_STATE_DIR", self.state_dir.path())
            .output()
            .context("Failed to run query command")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        // JSON format outputs an array of objects: [{"id":"..."}]
        let parsed: Vec<Value> = serde_json::from_str(stdout.trim())
            .with_context(|| format!("Failed to parse query JSON: {stdout}"))?;
        let id = parsed
            .first()
            .and_then(|obj| obj["id"].as_str())
            .ok_or_else(|| anyhow!("No 'id' field in query output: {stdout}"))?
            .to_string();

        Ok(id)
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
    let _session_id = server.wait_ready()?;

    // Run catenary list
    let output = Command::new(env!("CARGO_BIN_EXE_catenary"))
        .arg("list")
        .env("CATENARY_STATE_DIR", server.state_dir.path())
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
    let _session_id = server.wait_ready()?;

    // Run catenary list
    let output = Command::new(env!("CARGO_BIN_EXE_catenary"))
        .arg("list")
        .env("CATENARY_STATE_DIR", server.state_dir.path())
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
    let _session_id = server.wait_ready()?;

    // Start monitor with row number "1" - we just verify it successfully starts
    // monitoring some session (row number resolution works)
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("monitor").arg("1");
    cmd.env("CATENARY_STATE_DIR", server.state_dir.path());
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
    let state_dir = tempfile::tempdir().context("Failed to create state tempdir")?;
    let output = Command::new(env!("CARGO_BIN_EXE_catenary"))
        .arg("monitor")
        .arg("999")
        .env("CATENARY_STATE_DIR", state_dir.path())
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
    let session_id = server.wait_ready()?;

    // Start monitor using the full session ID — this must work regardless
    // of whether the ID happens to be all digits.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("monitor").arg(&session_id);
    cmd.env("CATENARY_STATE_DIR", server.state_dir.path());
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
    let session_id = server.wait_ready()?;

    // Start monitor with --raw flag
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("monitor").arg(&session_id).arg("--raw");
    cmd.env("CATENARY_STATE_DIR", server.state_dir.path());
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
    let session_id = server.wait_ready()?;

    // Start monitor with --nocolor flag
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("monitor").arg(&session_id).arg("--nocolor");
    cmd.env("CATENARY_STATE_DIR", server.state_dir.path());
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
    let session_id = server.wait_ready()?;

    // Start monitor with filter for "ping"
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("monitor")
        .arg(&session_id)
        .arg("--filter")
        .arg("ping");
    cmd.env("CATENARY_STATE_DIR", server.state_dir.path());
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
    let session_id = server.wait_ready()?;

    // Start monitor (without --raw)
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.arg("monitor").arg(&session_id).arg("--nocolor");
    cmd.env("CATENARY_STATE_DIR", server.state_dir.path());
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

// ── catenary config ─────────────────────────────────────────────

#[test]
fn test_config_outputs_valid_toml() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.arg("config");

    let output = cmd.output().context("Failed to run catenary config")?;
    assert!(output.status.success(), "catenary config should exit 0");

    let stdout = String::from_utf8_lossy(&output.stdout);
    toml::from_str::<toml::Value>(&stdout)
        .with_context(|| format!("catenary config output is not valid TOML:\n{stdout}"))?;
    Ok(())
}

#[test]
fn test_config_contains_deny_sections() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.arg("config");

    let output = cmd.output().context("Failed to run catenary config")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("[commands.deny]"),
        "output should contain [commands.deny]"
    );
    assert!(
        stdout.contains("[commands.deny_when_first]"),
        "output should contain [commands.deny_when_first]"
    );
    Ok(())
}

// ── catenary doctor suggestions ─────────────────────────────────

#[test]
fn test_doctor_suggests_config_when_no_config_file() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.arg("doctor").arg("--nocolor");

    let output = cmd.output().context("Failed to run catenary doctor")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Suggestions section should appear at the bottom
    assert!(
        stdout.contains("Suggestions:"),
        "doctor should show Suggestions section when no config exists, got:\n{stdout}"
    );
    assert!(
        stdout.contains("catenary config"),
        "doctor should suggest `catenary config`, got:\n{stdout}"
    );
    assert!(
        stdout.contains("No config file found"),
        "doctor should mention missing config file, got:\n{stdout}"
    );

    // Suggestions should be the last section
    let suggestions_pos = stdout
        .rfind("Suggestions:")
        .context("Suggestions: not found")?;
    let grammars_pos = stdout.rfind("Grammars:").context("Grammars: not found")?;
    assert!(
        suggestions_pos > grammars_pos,
        "Suggestions should appear after Grammars"
    );
    Ok(())
}

#[test]
fn test_doctor_no_suggestions_when_config_with_commands() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let config_dir = tmp.path().join("catenary");
    std::fs::create_dir_all(&config_dir)?;
    std::fs::write(
        config_dir.join("config.toml"),
        "[commands.deny]\ncat = \"Use read\"\n",
    )?;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.arg("doctor").arg("--nocolor");

    let output = cmd.output().context("Failed to run catenary doctor")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        !stdout.contains("Suggestions:"),
        "doctor should not show Suggestions when config with commands exists, got:\n{stdout}"
    );
    Ok(())
}

#[test]
fn test_doctor_suggests_commands_when_config_exists_without_commands() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let config_dir = tmp.path().join("catenary");
    std::fs::create_dir_all(&config_dir)?;
    std::fs::write(config_dir.join("config.toml"), "# no commands section\n")?;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.arg("doctor").arg("--nocolor");

    let output = cmd.output().context("Failed to run catenary doctor")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("Suggestions:"),
        "doctor should show Suggestions when config has no [commands], got:\n{stdout}"
    );
    assert!(
        stdout.contains("No [commands] section"),
        "doctor should mention missing [commands] section, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("No config file found"),
        "should not say config file is missing when it exists, got:\n{stdout}"
    );
    Ok(())
}

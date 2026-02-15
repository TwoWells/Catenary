#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Integration tests for the `run` shell execution tool.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// Helper to spawn the bridge with a config file.
struct BridgeProcess {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<BufReader<std::process::ChildStdout>>,
}

impl BridgeProcess {
    fn spawn_with_config(root: &str, config: &str) -> Result<Self> {
        // Write config to a temp file
        let config_path = std::path::Path::new(root).join(".catenary.toml");
        std::fs::write(&config_path, config)?;

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        cmd.arg("--root")
            .arg(root)
            .arg("--config")
            .arg(&config_path);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn().context("Failed to spawn bridge")?;
        let stdin = child.stdin.take().context("Failed to get stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("Failed to get stdout")?);

        std::thread::sleep(Duration::from_millis(200));

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
        })
    }

    fn spawn_without_config(root: &str) -> Result<Self> {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        cmd.arg("--root").arg(root);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn().context("Failed to spawn bridge")?;
        let stdin = child.stdin.take().context("Failed to get stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("Failed to get stdout")?);

        std::thread::sleep(Duration::from_millis(200));

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
        })
    }

    fn send(&mut self, request: &Value) -> Result<()> {
        let json = serde_json::to_string(request)?;
        let stdin = self.stdin.as_mut().context("Stdin already closed")?;
        writeln!(stdin, "{json}").context("Failed to write to stdin")?;
        stdin.flush().context("Failed to flush stdin")?;
        Ok(())
    }

    fn recv(&mut self) -> Result<Value> {
        let mut line = String::new();
        let stdout = self.stdout.as_mut().context("Stdout already closed")?;
        stdout
            .read_line(&mut line)
            .context("Failed to read from stdout")?;
        serde_json::from_str(&line).context("Failed to parse JSON response")
    }

    fn initialize(&mut self) -> Result<()> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "run-tool-test",
                    "version": "1.0.0"
                }
            }
        }))?;

        let response = self.recv()?;
        if response.get("result").is_none() {
            bail!("Initialize failed: {response:?}");
        }

        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))?;

        std::thread::sleep(Duration::from_millis(100));
        Ok(())
    }

    fn call_tool(&mut self, name: &str, args: Value) -> Result<Value> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": args
            }
        }))?;
        let response = self.recv()?;
        let result = response
            .get("result")
            .context("No result in response")?
            .clone();
        Ok(result)
    }

    fn call_tool_text(&mut self, name: &str, args: Value) -> Result<String> {
        let result = self.call_tool(name, args)?;
        let content = result
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|item| item.get("text"))
            .and_then(|t| t.as_str())
            .context("No text content in result")?;
        Ok(content.to_string())
    }
}

impl Drop for BridgeProcess {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.wait();
    }
}

#[test]
fn test_run_allowed_command() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let mut bridge = BridgeProcess::spawn_with_config(
        &dir.path().to_string_lossy(),
        r#"
[tools.run]
allowed = ["echo"]
"#,
    )?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "run",
        json!({ "command": "echo", "args": ["hello", "world"] }),
    )?;

    assert!(
        text.contains("hello world"),
        "Should contain echo output: {text}"
    );
    assert!(
        text.contains("Exit code: 0"),
        "Should show exit code: {text}"
    );
    Ok(())
}

#[test]
fn test_run_denied_command() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let mut bridge = BridgeProcess::spawn_with_config(
        &dir.path().to_string_lossy(),
        r#"
[tools.run]
allowed = ["echo"]
"#,
    )?;
    bridge.initialize()?;

    let result = bridge.call_tool("run", json!({ "command": "rm", "args": ["-rf", "/"] }))?;

    let is_error = result.get("isError").and_then(|v| v.as_bool());
    assert_eq!(is_error, Some(true), "Should be an error");

    let text = result
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    assert!(
        text.contains("not allowed"),
        "Error should mention not allowed: {text}"
    );
    assert!(
        text.contains("echo"),
        "Error should include allowlist: {text}"
    );
    Ok(())
}

#[test]
fn test_run_unrestricted() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let mut bridge = BridgeProcess::spawn_with_config(
        &dir.path().to_string_lossy(),
        r#"
[tools.run]
allowed = ["*"]
"#,
    )?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "run",
        json!({ "command": "echo", "args": ["unrestricted"] }),
    )?;

    assert!(text.contains("unrestricted"), "Should work: {text}");
    Ok(())
}

#[test]
fn test_run_not_registered_without_config() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let mut bridge = BridgeProcess::spawn_without_config(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Check tools/list
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    }))?;

    let response = bridge.recv()?;
    let tools = response
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .context("No tools in response")?;

    let tool_names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();

    assert!(
        !tool_names.contains(&"run"),
        "run tool should NOT appear when not configured: {tool_names:?}"
    );
    Ok(())
}

#[test]
fn test_run_timeout() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let mut bridge = BridgeProcess::spawn_with_config(
        &dir.path().to_string_lossy(),
        r#"
[tools.run]
allowed = ["sleep"]
"#,
    )?;
    bridge.initialize()?;

    let result = bridge.call_tool(
        "run",
        json!({ "command": "sleep", "args": ["10"], "timeout": 1 }),
    )?;

    let is_error = result.get("isError").and_then(|v| v.as_bool());
    assert_eq!(is_error, Some(true), "Should be an error (timeout)");

    let text = result
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    assert!(text.contains("TIMED OUT"), "Should mention timeout: {text}");
    Ok(())
}

#[test]
fn test_run_dynamic_description_includes_allowlist() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let mut bridge = BridgeProcess::spawn_with_config(
        &dir.path().to_string_lossy(),
        r#"
[tools.run]
allowed = ["git", "make"]
"#,
    )?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    }))?;

    let response = bridge.recv()?;
    let tools = response
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .context("No tools in response")?;

    let run_tool = tools
        .iter()
        .find(|t| t.get("name").and_then(|n| n.as_str()) == Some("run"))
        .context("run tool not found")?;

    let description = run_tool
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");

    assert!(
        description.contains("git"),
        "Description should include git: {description}"
    );
    assert!(
        description.contains("make"),
        "Description should include make: {description}"
    );
    Ok(())
}

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Integration tests for file I/O tools: `read_file`, `write_file`, `edit_file`, `list_directory`.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// Helper to spawn the bridge and communicate with it.
struct BridgeProcess {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<BufReader<std::process::ChildStdout>>,
}

impl BridgeProcess {
    fn spawn(root: &str) -> Result<Self> {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        cmd.arg("--root").arg(root);
        // Isolate from user-level config
        cmd.env("XDG_CONFIG_HOME", root);
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
                    "name": "file-tools-test",
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

    fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value> {
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

    fn call_tool_text(&mut self, name: &str, args: &Value) -> Result<String> {
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
        // Close stdin to trigger shutdown
        self.stdin.take();
        let _ = self.child.wait();
    }
}

#[test]
fn test_read_file_basic() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_path = dir.path().join("hello.txt");
    std::fs::write(&file_path, "line one\nline two\nline three\n")?;

    let mut bridge = BridgeProcess::spawn(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "read_file",
        &json!({ "file": file_path.to_string_lossy().to_string() }),
    )?;

    assert!(text.contains("line one"), "Should contain file content");
    assert!(text.contains("line two"), "Should contain file content");
    assert!(text.contains("line three"), "Should contain file content");
    // Should have line numbers
    assert!(text.contains("1\t"), "Should have line numbers");
    Ok(())
}

#[test]
fn test_read_file_with_offset_limit() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_path = dir.path().join("lines.txt");
    std::fs::write(&file_path, "line 1\nline 2\nline 3\nline 4\nline 5\n")?;

    let mut bridge = BridgeProcess::spawn(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "read_file",
        &json!({ "file": file_path.to_string_lossy().to_string(), "offset": 2, "limit": 2 }),
    )?;

    assert!(text.contains("line 2"), "Should contain line 2");
    assert!(text.contains("line 3"), "Should contain line 3");
    assert!(!text.contains("line 1"), "Should not contain line 1");
    assert!(!text.contains("line 4"), "Should not contain line 4");
    Ok(())
}

#[test]
fn test_read_file_outside_root_fails() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let mut bridge = BridgeProcess::spawn(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let result = bridge.call_tool("read_file", &json!({ "file": "/etc/hostname" }))?;

    let is_error = result.get("isError").and_then(serde_json::Value::as_bool);
    assert_eq!(is_error, Some(true), "Should be an error");

    let text = result
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    assert!(
        text.contains("outside workspace roots"),
        "Error should mention workspace roots: {text}"
    );
    Ok(())
}

#[test]
fn test_write_file_creates_file() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_path = dir.path().join("new_file.txt");

    let mut bridge = BridgeProcess::spawn(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "write_file",
        &json!({
            "file": file_path.to_string_lossy().to_string(),
            "content": "hello world\nsecond line\n"
        }),
    )?;

    assert!(
        text.contains("Wrote 2 lines"),
        "Should report line count: {text}"
    );

    // Verify file was actually written
    let content = std::fs::read_to_string(&file_path)?;
    assert_eq!(content, "hello world\nsecond line\n");
    Ok(())
}

#[test]
fn test_write_file_creates_parent_dirs() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_path = dir.path().join("a/b/c/deep_file.txt");

    let mut bridge = BridgeProcess::spawn(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "write_file",
        &json!({
            "file": file_path.to_string_lossy().to_string(),
            "content": "deep content\n"
        }),
    )?;

    assert!(text.contains("Wrote"), "Should report success: {text}");
    assert!(file_path.exists(), "File should exist");
    Ok(())
}

#[test]
fn test_write_file_outside_root_fails() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let mut bridge = BridgeProcess::spawn(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let result = bridge.call_tool(
        "write_file",
        &json!({ "file": "/tmp/outside_root.txt", "content": "hack" }),
    )?;

    let is_error = result.get("isError").and_then(serde_json::Value::as_bool);
    assert_eq!(is_error, Some(true), "Should be an error");
    Ok(())
}

#[test]
fn test_write_file_config_protection() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let config_path = dir.path().join(".catenary.toml");
    std::fs::write(&config_path, "idle_timeout = 300")?;

    let mut bridge = BridgeProcess::spawn(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let result = bridge.call_tool(
        "write_file",
        &json!({
            "file": config_path.to_string_lossy().to_string(),
            "content": "idle_timeout = 0\n"
        }),
    )?;

    let is_error = result.get("isError").and_then(serde_json::Value::as_bool);
    assert_eq!(is_error, Some(true), "Should be an error for config file");

    // Verify file was NOT modified
    let content = std::fs::read_to_string(&config_path)?;
    assert_eq!(
        content, "idle_timeout = 300",
        "Config should not be modified"
    );
    Ok(())
}

#[test]
fn test_edit_file_basic() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_path = dir.path().join("edit_me.txt");
    std::fs::write(&file_path, "Hello World\nFoo Bar\n")?;

    let mut bridge = BridgeProcess::spawn(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "edit_file",
        &json!({
            "file": file_path.to_string_lossy().to_string(),
            "old_string": "Foo Bar",
            "new_string": "Baz Qux"
        }),
    )?;

    assert!(text.contains("Edited"), "Should report success: {text}");

    let content = std::fs::read_to_string(&file_path)?;
    assert!(content.contains("Baz Qux"), "Should contain new text");
    assert!(!content.contains("Foo Bar"), "Should not contain old text");
    Ok(())
}

#[test]
fn test_edit_file_not_found() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_path = dir.path().join("edit_me.txt");
    std::fs::write(&file_path, "Hello World\n")?;

    let mut bridge = BridgeProcess::spawn(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let result = bridge.call_tool(
        "edit_file",
        &json!({
            "file": file_path.to_string_lossy().to_string(),
            "old_string": "nonexistent text",
            "new_string": "replacement"
        }),
    )?;

    let is_error = result.get("isError").and_then(serde_json::Value::as_bool);
    assert_eq!(is_error, Some(true), "Should be an error");
    Ok(())
}

#[test]
fn test_edit_file_ambiguous() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_path = dir.path().join("ambig.txt");
    std::fs::write(&file_path, "foo\nfoo\nbar\n")?;

    let mut bridge = BridgeProcess::spawn(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let result = bridge.call_tool(
        "edit_file",
        &json!({
            "file": file_path.to_string_lossy().to_string(),
            "old_string": "foo",
            "new_string": "baz"
        }),
    )?;

    let is_error = result.get("isError").and_then(serde_json::Value::as_bool);
    assert_eq!(
        is_error,
        Some(true),
        "Should be an error for ambiguous match"
    );

    let text = result
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    assert!(
        text.contains("2 times"),
        "Error should mention multiple matches: {text}"
    );
    Ok(())
}

#[test]
fn test_list_directory_basic() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::create_dir_all(dir.path().join("src"))?;
    std::fs::write(dir.path().join("Cargo.toml"), "[package]")?;
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}")?;

    let mut bridge = BridgeProcess::spawn(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "list_directory",
        &json!({ "path": dir.path().to_string_lossy().to_string() }),
    )?;

    assert!(text.contains("src/"), "Should list src directory: {text}");
    assert!(
        text.contains("Cargo.toml"),
        "Should list Cargo.toml: {text}"
    );
    Ok(())
}

#[test]
fn test_list_directory_outside_root_fails() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let mut bridge = BridgeProcess::spawn(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let result = bridge.call_tool("list_directory", &json!({ "path": "/etc" }))?;

    let is_error = result.get("isError").and_then(serde_json::Value::as_bool);
    assert_eq!(is_error, Some(true), "Should be an error");
    Ok(())
}

#[test]
fn test_tools_list_includes_file_tools() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let mut bridge = BridgeProcess::spawn(&dir.path().to_string_lossy())?;
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

    let tool_names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();

    assert!(
        tool_names.contains(&"read_file"),
        "Should include read_file: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"write_file"),
        "Should include write_file: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"edit_file"),
        "Should include edit_file: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"list_directory"),
        "Should include list_directory: {tool_names:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn test_list_directory_symlink_shown_not_followed() -> Result<()> {
    use std::os::unix::fs as unix_fs;

    let dir = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;

    std::fs::write(outside.path().join("secret.txt"), "secret")?;

    // Create symlink inside workspace pointing outside
    unix_fs::symlink(
        outside.path().join("secret.txt"),
        dir.path().join("link.txt"),
    )?;

    let mut bridge = BridgeProcess::spawn(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "list_directory",
        &json!({ "path": dir.path().to_string_lossy().to_string() }),
    )?;

    // Symlink should be shown with its target
    assert!(
        text.contains("link.txt ->"),
        "Symlink should be shown with arrow: {text}"
    );
    Ok(())
}

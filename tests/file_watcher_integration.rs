// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for `workspace/didChangeWatchedFiles`.
//!
//! Validates the full pipeline: filesystem change → `diff()` →
//! registration match → `didChangeWatchedFiles` notification.
//! Uses mockls with `--register-file-watchers` and `--notification-log`
//! to capture and verify notification delivery.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

const MOCK_LANG_A: &str = "yX4Za";

// ── Bridge process helper ───────────────────────────────────────────

struct BridgeProcess {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<BufReader<std::process::ChildStdout>>,
    stderr: Option<std::process::ChildStderr>,
    /// Temp dir for XDG state/config, kept alive for the bridge lifetime.
    _state_dir: tempfile::TempDir,
}

impl BridgeProcess {
    fn spawn(lsp_commands: &[&str], root: &str) -> Result<Self> {
        // Isolate state/config from the workspace root so bridge-created
        // files (notify.sock, DB, etc.) don't appear in the filesystem diff.
        let state_dir = tempfile::tempdir().context("Failed to create state dir")?;
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        cmd.env("CATENARY_SERVERS", lsp_commands.join(";"));
        cmd.env("CATENARY_ROOTS", root);
        cmd.env("XDG_CONFIG_HOME", state_dir.path());
        cmd.env("XDG_STATE_HOME", state_dir.path());
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().context("Failed to spawn bridge")?;
        let stdin = child.stdin.take().context("Failed to get stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("Failed to get stdout")?);
        let stderr = child.stderr.take();

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr,
            _state_dir: state_dir,
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
        let n = stdout
            .read_line(&mut line)
            .context("Failed to read from stdout")?;
        if n == 0 {
            let mut stderr_buf = String::new();
            if let Some(ref mut stderr) = self.stderr {
                let _ = stderr.read_to_string(&mut stderr_buf);
            }
            let status = self.child.try_wait().ok().flatten();
            bail!(
                "bridge process closed stdout (EOF). exit status: {status:?}, stderr:\n{stderr_buf}"
            );
        }
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
                    "name": "file-watcher-test",
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

        // Allow time for mockls to process initialized + send registerCapability
        // and for the bridge to process the registration.
        std::thread::sleep(Duration::from_millis(300));
        Ok(())
    }

    /// Calls `grep` to trigger `notify_file_changes()` at the tool boundary.
    /// The search itself doesn't matter — we just need the diff to run.
    fn trigger_file_watch_diff(&mut self) -> Result<Value> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "tools/call",
            "params": {
                "name": "grep",
                "arguments": { "pattern": "NONEXISTENT_PATTERN_FOR_DIFF_TRIGGER" }
            }
        }))?;
        self.recv()
    }
}

impl Drop for BridgeProcess {
    fn drop(&mut self) {
        self.stdin.take();
        for _ in 0..20 {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn mockls_lsp_arg(lang: &str, flags: &str) -> String {
    let bin = env!("CARGO_BIN_EXE_mockls");
    if flags.is_empty() {
        format!("{lang}:{bin} {lang}")
    } else {
        format!("{lang}:{bin} {lang} {flags}")
    }
}

/// Reads the JSONL notification log and returns all `didChangeWatchedFiles`
/// entries with their changes arrays.
fn read_watched_file_changes(log_path: &std::path::Path) -> Vec<Vec<(String, u64)>> {
    let log = std::fs::read_to_string(log_path).unwrap_or_default();
    let mut results = Vec::new();
    for line in log.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("method").and_then(Value::as_str) != Some("workspace/didChangeWatchedFiles") {
            continue;
        }
        let changes = entry
            .get("changes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let parsed: Vec<(String, u64)> = changes
            .iter()
            .filter_map(|c| {
                let uri = c.get("uri")?.as_str()?.to_string();
                let typ = c.get("type")?.as_u64()?;
                Some((uri, typ))
            })
            .collect();
        results.push(parsed);
    }
    results
}

/// Checks whether *any* notification batch contains the given URI and type.
fn has_change(batches: &[Vec<(String, u64)>], uri_suffix: &str, change_type: u64) -> bool {
    batches
        .iter()
        .flat_map(|b| b.iter())
        .any(|(uri, typ)| uri.ends_with(uri_suffix) && *typ == change_type)
}

/// Sets a file's mtime to 10 seconds in the past.
/// This ensures that a subsequent write will produce a detectable mtime change,
/// since `FilesystemManager::diff()` compares mtimes at second resolution.
fn backdate_mtime(path: &std::path::Path) {
    let past = SystemTime::now() - Duration::from_secs(10);
    let times = std::fs::FileTimes::new().set_modified(past);
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open for backdate");
    file.set_times(times).expect("set mtime");
}

// FileChangeType constants matching the LSP spec.
const CREATED: u64 = 1;
const CHANGED: u64 = 2;
const DELETED: u64 = 3;

// ── Tests ───────────────────────────────────────────────────────────

/// No filesystem changes → no `didChangeWatchedFiles` notification.
#[test]
fn noop_diff_sends_no_notification() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_dir = tempfile::tempdir()?;
    let log_path = log_dir.path().join("notifications.jsonl");
    let test_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn hello()\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--register-file-watchers --notification-log {log_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // No filesystem changes — just trigger a diff
    let _ = bridge.trigger_file_watch_diff()?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(200));

    let batches = read_watched_file_changes(&log_path);
    assert!(
        batches.is_empty(),
        "Expected no didChangeWatchedFiles, got: {batches:?}"
    );
    Ok(())
}

/// Creating a new file → `Created` event in notification.
#[test]
fn new_file_sends_created_event() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_dir = tempfile::tempdir()?;
    let log_path = log_dir.path().join("notifications.jsonl");
    // Seed file so the language is detected and mockls spawns
    let seed_file = dir.path().join(format!("seed.{MOCK_LANG_A}"));
    std::fs::write(&seed_file, "fn seed()\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--register-file-watchers --notification-log {log_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Create a new file after seed
    let new_file = dir.path().join(format!("new_module.{MOCK_LANG_A}"));
    std::fs::write(&new_file, "fn new_thing()\n")?;

    let _ = bridge.trigger_file_watch_diff()?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(200));

    let batches = read_watched_file_changes(&log_path);
    assert!(
        has_change(&batches, &format!("new_module.{MOCK_LANG_A}"), CREATED),
        "Expected Created event for new_module. Batches: {batches:?}"
    );
    Ok(())
}

/// Deleting a file → `Deleted` event in notification.
#[test]
fn deleted_file_sends_deleted_event() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_dir = tempfile::tempdir()?;
    let log_path = log_dir.path().join("notifications.jsonl");
    let doomed_file = dir.path().join(format!("doomed.{MOCK_LANG_A}"));
    std::fs::write(&doomed_file, "fn doomed()\n")?;
    // Keep a seed file so the language stays active
    let seed_file = dir.path().join(format!("seed.{MOCK_LANG_A}"));
    std::fs::write(&seed_file, "fn seed()\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--register-file-watchers --notification-log {log_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Delete the file
    std::fs::remove_file(&doomed_file)?;

    let _ = bridge.trigger_file_watch_diff()?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(200));

    let batches = read_watched_file_changes(&log_path);
    assert!(
        has_change(&batches, &format!("doomed.{MOCK_LANG_A}"), DELETED),
        "Expected Deleted event for doomed. Batches: {batches:?}"
    );
    Ok(())
}

/// Creating a directory → `Created` event in notification.
#[test]
fn new_directory_sends_created_event() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_dir = tempfile::tempdir()?;
    let log_path = log_dir.path().join("notifications.jsonl");
    let seed_file = dir.path().join(format!("seed.{MOCK_LANG_A}"));
    std::fs::write(&seed_file, "fn seed()\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--register-file-watchers --notification-log {log_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Create a new subdirectory
    std::fs::create_dir(dir.path().join("subdir"))?;

    let _ = bridge.trigger_file_watch_diff()?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(200));

    let batches = read_watched_file_changes(&log_path);
    assert!(
        has_change(&batches, "subdir", CREATED),
        "Expected Created event for subdir. Batches: {batches:?}"
    );
    Ok(())
}

/// Simulates `git checkout` — multiple creates, deletes, and changes in one
/// diff cycle produce a single batched notification.
#[test]
fn branch_switch_sends_batched_notification() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_dir = tempfile::tempdir()?;
    let log_path = log_dir.path().join("notifications.jsonl");
    let file_a = dir.path().join(format!("a.{MOCK_LANG_A}"));
    let file_b = dir.path().join(format!("b.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn a()\n")?;
    std::fs::write(&file_b, "fn b()\n")?;
    // Backdate mtime so that the rewrite below produces a detectable change
    // (diff() compares mtimes at second resolution).
    backdate_mtime(&file_b);

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--register-file-watchers --notification-log {log_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Simulate branch switch: delete a, modify b, create c
    std::fs::remove_file(&file_a)?;
    std::fs::write(&file_b, "fn b_modified()\n")?;
    let file_c = dir.path().join(format!("c.{MOCK_LANG_A}"));
    std::fs::write(&file_c, "fn c()\n")?;

    let _ = bridge.trigger_file_watch_diff()?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(200));

    let batches = read_watched_file_changes(&log_path);
    assert!(
        has_change(&batches, &format!("a.{MOCK_LANG_A}"), DELETED),
        "Expected Deleted event for a. Batches: {batches:?}"
    );
    assert!(
        has_change(&batches, &format!("b.{MOCK_LANG_A}"), CHANGED),
        "Expected Changed event for b. Batches: {batches:?}"
    );
    assert!(
        has_change(&batches, &format!("c.{MOCK_LANG_A}"), CREATED),
        "Expected Created event for c. Batches: {batches:?}"
    );
    Ok(())
}

/// Module rename (the motivating case): `mv foo.ext foo/mod.ext` should
/// produce `Deleted(foo.ext)` + `Created(foo/mod.ext)`.
#[test]
fn module_rename_sends_delete_and_create() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_dir = tempfile::tempdir()?;
    let log_path = log_dir.path().join("notifications.jsonl");
    let foo_file = dir.path().join(format!("foo.{MOCK_LANG_A}"));
    std::fs::write(&foo_file, "fn foo()\n")?;
    // Keep a seed file so the language stays active after rename
    let seed_file = dir.path().join(format!("seed.{MOCK_LANG_A}"));
    std::fs::write(&seed_file, "fn seed()\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--register-file-watchers --notification-log {log_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Rename: foo.ext → foo/mod.ext
    let foo_dir = dir.path().join("foo");
    std::fs::create_dir(&foo_dir)?;
    let mod_file = foo_dir.join(format!("mod.{MOCK_LANG_A}"));
    std::fs::rename(&foo_file, &mod_file)?;

    let _ = bridge.trigger_file_watch_diff()?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(200));

    let batches = read_watched_file_changes(&log_path);
    assert!(
        has_change(&batches, &format!("foo.{MOCK_LANG_A}"), DELETED),
        "Expected Deleted event for foo.ext. Batches: {batches:?}"
    );
    assert!(
        has_change(&batches, &format!("foo/mod.{MOCK_LANG_A}"), CREATED),
        "Expected Created event for foo/mod.ext. Batches: {batches:?}"
    );
    Ok(())
}

/// Watch kind filtering: a watcher with `kind=4` (Delete only) should not
/// fire for Created events, but should fire for Deleted events.
#[test]
fn watch_kind_delete_only_filters_creates() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_dir = tempfile::tempdir()?;
    let log_path = log_dir.path().join("notifications.jsonl");
    let doomed_file = dir.path().join(format!("doomed.{MOCK_LANG_A}"));
    std::fs::write(&doomed_file, "fn doomed()\n")?;
    let seed_file = dir.path().join(format!("seed.{MOCK_LANG_A}"));
    std::fs::write(&seed_file, "fn seed()\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--register-file-watchers --watcher-kind 4 --notification-log {log_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Create a new file — should NOT trigger (kind=4 is Delete only)
    let new_file = dir.path().join(format!("new.{MOCK_LANG_A}"));
    std::fs::write(&new_file, "fn new()\n")?;

    let _ = bridge.trigger_file_watch_diff()?;

    // Now delete a file — should trigger
    std::fs::remove_file(&doomed_file)?;

    let _ = bridge.trigger_file_watch_diff()?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(200));

    let batches = read_watched_file_changes(&log_path);
    assert!(
        !has_change(&batches, &format!("new.{MOCK_LANG_A}"), CREATED),
        "Created event should NOT appear with kind=4 (Delete only). Batches: {batches:?}"
    );
    assert!(
        has_change(&batches, &format!("doomed.{MOCK_LANG_A}"), DELETED),
        "Deleted event SHOULD appear with kind=4. Batches: {batches:?}"
    );
    Ok(())
}

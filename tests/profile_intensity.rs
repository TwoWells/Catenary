// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
#![allow(
    clippy::print_stdout,
    reason = "profiling test prints summary to stdout"
)]
#![allow(
    clippy::similar_names,
    reason = "utime/stime/pfc counter names are intentionally similar"
)]
//! Intensity profiling integration test.
//!
//! Spawns real LSP servers against fixture projects, runs the settle loop
//! with a recording sink, writes samples to a temp `SQLite` database, and
//! prints per-server summary statistics.
//!
//! Run with:
//! ```text
//! make test-ignored T=profile_intensity
//! ```

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rusqlite::params;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio_util::sync::CancellationToken;

use catenary_mcp::lsp::LspServer;
use catenary_mcp::lsp::settle::{SettleSample, SettleSink, settle_loop};

// ── Constants ────────────────────────────────────────────────────────

const SAMPLE_INTERVAL: Duration = Duration::from_millis(50);
const IDLE_DURATION: Duration = Duration::from_secs(5);
const STIMULUS_DURATION: Duration = Duration::from_secs(10);
const RECOVERY_DURATION: Duration = Duration::from_secs(10);

// ── Server definitions ───────────────────────────────────────────────

struct ServerDef {
    name: &'static str,
    binary: &'static str,
    args: &'static [&'static str],
    language_id: &'static str,
    fixture_dir: &'static str,
    bad_file: &'static str,
    clean_file: Option<&'static str>,
}

const SERVERS: &[ServerDef] = &[
    ServerDef {
        name: "rust-analyzer",
        binary: "rust-analyzer",
        args: &[],
        language_id: "rust",
        fixture_dir: "rust",
        bad_file: "src/main.rs",
        clean_file: Some("src/clean.rs"),
    },
    ServerDef {
        name: "taplo",
        binary: "taplo",
        args: &["lsp", "stdio"],
        language_id: "toml",
        fixture_dir: "toml",
        bad_file: "bad.toml",
        clean_file: None,
    },
    ServerDef {
        name: "marksman",
        binary: "marksman",
        args: &["server"],
        language_id: "markdown",
        fixture_dir: "markdown",
        bad_file: "doc.md",
        clean_file: None,
    },
    ServerDef {
        name: "bash-language-server",
        binary: "bash-language-server",
        args: &["start"],
        language_id: "shellscript",
        fixture_dir: "bash",
        bad_file: "bad.sh",
        clean_file: None,
    },
    ServerDef {
        name: "vscode-json-language-server",
        binary: "vscode-json-language-server",
        args: &["--stdio"],
        language_id: "json",
        fixture_dir: "json",
        bad_file: "bad.json",
        clean_file: None,
    },
    ServerDef {
        name: "pyright-langserver",
        binary: "pyright-langserver",
        args: &["--stdio"],
        language_id: "python",
        fixture_dir: "python",
        bad_file: "bad.py",
        clean_file: None,
    },
    ServerDef {
        name: "yaml-language-server",
        binary: "yaml-language-server",
        args: &["--stdio"],
        language_id: "yaml",
        fixture_dir: "yaml",
        bad_file: "bad.yaml",
        clean_file: None,
    },
];

// ── Recording sink ───────────────────────────────────────────────────

struct RecordingSink {
    db: rusqlite::Connection,
    start: Instant,
}

impl RecordingSink {
    fn new(path: &Path) -> Result<Self> {
        let db = rusqlite::Connection::open(path)?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS intensity_samples (
                id            INTEGER PRIMARY KEY,
                timestamp_ms  INTEGER NOT NULL,
                server        TEXT    NOT NULL,
                pid           INTEGER NOT NULL,
                ppid          INTEGER NOT NULL,
                delta_pfc     INTEGER NOT NULL,
                delta_utime   INTEGER NOT NULL,
                delta_stime   INTEGER NOT NULL,
                in_progress   INTEGER NOT NULL,
                process_count INTEGER NOT NULL
            );",
        )?;
        Ok(Self {
            db,
            start: Instant::now(),
        })
    }
}

impl SettleSink for RecordingSink {
    fn record(&mut self, sample: &SettleSample) -> bool {
        let elapsed_ms = sample.timestamp.duration_since(self.start).as_millis();

        #[allow(
            clippy::cast_possible_wrap,
            clippy::cast_possible_truncation,
            reason = "sample values and elapsed ms fit in i64"
        )]
        let result = self.db.execute(
            "INSERT INTO intensity_samples
             (timestamp_ms, server, pid, ppid, delta_pfc, delta_utime, delta_stime, in_progress, process_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                elapsed_ms as i64,
                sample.server,
                sample.pid,
                sample.ppid,
                sample.delta_pfc as i64,
                sample.delta_utime as i64,
                sample.delta_stime as i64,
                sample.in_progress_count,
                sample.process_count as i64,
            ],
        );
        // Recording failure shouldn't crash the loop — just stop.
        result.is_ok()
    }
}

// ── LSP transport helpers ────────────────────────────────────────────

async fn send_lsp(stdin: &mut ChildStdin, msg: &Value) -> Result<()> {
    let body = serde_json::to_string(msg)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    stdin.write_all(header.as_bytes()).await?;
    stdin.write_all(body.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

async fn recv_lsp(reader: &mut BufReader<ChildStdout>) -> Result<Value> {
    // Read headers until blank line.
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .context("Failed to read LSP header line")?;

        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if trimmed.to_ascii_lowercase().starts_with("content-length:")
            && let Some(val) = trimmed.split(':').nth(1)
        {
            content_length = val.trim().parse().ok();
        }
    }

    let len = content_length.context("Missing Content-Length header")?;
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .await
        .context("Failed to read LSP body")?;
    serde_json::from_slice(&body).context("Failed to parse LSP JSON")
}

/// Drain any pending messages from the server (non-blocking).
async fn drain_lsp(reader: &mut BufReader<ChildStdout>) {
    while let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(100), recv_lsp(reader)).await {
    }
}

// ── Binary lookup ────────────────────────────────────────────────────

fn find_binary(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(name);
            if candidate.is_file() {
                Some(candidate)
            } else {
                None
            }
        })
    })
}

// ── LSP handshake ────────────────────────────────────────────────────

async fn initialize_server(
    stdin: &mut ChildStdin,
    reader: &mut BufReader<ChildStdout>,
    root_uri: &str,
) -> Result<Value> {
    let init_params = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": {
                        "versionSupport": true
                    },
                    "diagnostic": {
                        "dynamicRegistration": false
                    }
                },
                "window": {
                    "workDoneProgress": true
                }
            },
            "workspaceFolders": [{
                "uri": root_uri,
                "name": "profile"
            }]
        }
    });

    send_lsp(stdin, &init_params).await?;

    // Read messages until we get the initialize response (id: 1).
    let capabilities = loop {
        let msg = tokio::time::timeout(Duration::from_secs(30), recv_lsp(reader))
            .await
            .context("Timeout waiting for initialize response")?
            .context("Failed to read initialize response")?;

        if msg.get("id").and_then(Value::as_i64) == Some(1) {
            break msg
                .get("result")
                .and_then(|r| r.get("capabilities"))
                .cloned()
                .unwrap_or_default();
        }
        // Otherwise it's a notification or server request — skip.
    };

    // Send initialized notification.
    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    });
    send_lsp(stdin, &initialized).await?;

    Ok(capabilities)
}

// ── Shutdown ─────────────────────────────────────────────────────────

async fn shutdown_server(
    stdin: &mut ChildStdin,
    reader: &mut BufReader<ChildStdout>,
    child: &mut Child,
) {
    // Send shutdown request.
    let shutdown = json!({
        "jsonrpc": "2.0",
        "id": 9999,
        "method": "shutdown",
        "params": null
    });
    let _ = send_lsp(stdin, &shutdown).await;

    // Try to read shutdown response with timeout.
    let _ = tokio::time::timeout(Duration::from_secs(5), recv_lsp(reader)).await;

    // Send exit notification.
    let exit = json!({
        "jsonrpc": "2.0",
        "method": "exit",
        "params": null
    });
    let _ = send_lsp(stdin, &exit).await;

    // Give the process a moment to exit gracefully, then kill.
    match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(_) => {}
        Err(_) => {
            let _ = child.kill().await;
        }
    }
}

// ── Summary output ───────────────────────────────────────────────────

fn print_summary(db_path: &Path) -> Result<()> {
    let db = rusqlite::Connection::open(db_path)?;

    let mut stmt = db.prepare("SELECT DISTINCT server FROM intensity_samples ORDER BY server")?;
    let servers: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .filter_map(Result::ok)
        .collect();

    println!("\n{}", "=".repeat(60));
    println!("  INTENSITY PROFILING SUMMARY");
    println!("{}\n", "=".repeat(60));

    for server in &servers {
        println!("--- {server} ---\n");

        // Total samples
        let total: i64 = db.query_row(
            "SELECT COUNT(*) FROM intensity_samples WHERE server = ?1",
            params![server],
            |row| row.get(0),
        )?;
        println!("  Total samples: {total}");

        // Max process count
        let max_procs: i64 = db.query_row(
            "SELECT COALESCE(MAX(process_count), 0) FROM intensity_samples WHERE server = ?1",
            params![server],
            |row| row.get(0),
        )?;
        println!("  Max process count: {max_procs}");

        // Aggregate stats
        let (sum_pfc, sum_utime, sum_stime, max_pfc, max_utime): (i64, i64, i64, i64, i64) = db
            .query_row(
                "SELECT
                    COALESCE(SUM(delta_pfc), 0),
                    COALESCE(SUM(delta_utime), 0),
                    COALESCE(SUM(delta_stime), 0),
                    COALESCE(MAX(delta_pfc), 0),
                    COALESCE(MAX(delta_utime), 0)
                 FROM intensity_samples WHERE server = ?1",
                params![server],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?;

        println!("  Total delta_pfc: {sum_pfc}  (max single: {max_pfc})");
        println!("  Total delta_utime: {sum_utime}  (max single: {max_utime})");
        println!("  Total delta_stime: {sum_stime}");

        // In-progress transitions
        let progress_changes: i64 = db.query_row(
            "SELECT COUNT(*) FROM (
                SELECT in_progress, LAG(in_progress) OVER (ORDER BY id) AS prev
                FROM intensity_samples WHERE server = ?1
            ) WHERE in_progress != prev",
            params![server],
            |row| row.get(0),
        )?;
        println!("  in_progress transitions: {progress_changes}");

        // Distinct child PIDs
        let distinct_pids: i64 = db.query_row(
            "SELECT COUNT(DISTINCT pid) FROM intensity_samples WHERE server = ?1",
            params![server],
            |row| row.get(0),
        )?;
        println!("  Distinct PIDs observed: {distinct_pids}");

        println!();
    }

    Ok(())
}

// ── Main test ────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "manual profiling test — requires real LSP servers"]
#[allow(
    clippy::too_many_lines,
    reason = "sequential per-server profiling loop with setup/stimulus/teardown phases"
)]
async fn profile_intensity() -> Result<()> {
    let fixtures_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/profile");

    // PROFILE_DB=/path/to/output.db persists the database for later analysis.
    // Without it, a tempdir is used and cleaned up on exit.
    let tmp_dir_guard;
    let db_path = if let Ok(path) = std::env::var("PROFILE_DB") {
        tmp_dir_guard = None;
        let p = PathBuf::from(&path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        p
    } else {
        let td = tempfile::tempdir()?;
        let p = td.path().join("intensity.db");
        tmp_dir_guard = Some(td);
        p
    };
    // Keep tempdir alive for the duration of the test.
    let _ = &tmp_dir_guard;

    let mut any_server_ran = false;

    for def in SERVERS {
        // Check if binary is available.
        if find_binary(def.binary).is_none() {
            println!("SKIP {}: binary '{}' not found", def.name, def.binary);
            continue;
        }

        println!("PROFILING: {}", def.name);

        // Copy fixtures to a temp dir so we don't pollute the source tree.
        let work_dir = tempfile::tempdir()?;
        let fixture_src = fixtures_dir.join(def.fixture_dir);
        copy_dir_recursive(&fixture_src, work_dir.path())?;

        let root_uri = format!("file://{}", work_dir.path().display());

        // Spawn server.
        let mut child = Command::new(def.binary)
            .args(def.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .current_dir(work_dir.path())
            .spawn()
            .with_context(|| format!("Failed to spawn {}", def.name))?;

        let mut stdin = child.stdin.take().context("No stdin")?;
        let stdout = child.stdout.take().context("No stdout")?;
        let mut reader = BufReader::new(stdout);

        // Initialize.
        let capabilities = match tokio::time::timeout(
            Duration::from_secs(30),
            initialize_server(&mut stdin, &mut reader, &root_uri),
        )
        .await
        {
            Ok(Ok(caps)) => caps,
            Ok(Err(e)) => {
                println!("  SKIP: initialize failed: {e}");
                let _ = child.kill().await;
                continue;
            }
            Err(_) => {
                println!("  SKIP: initialize timed out");
                let _ = child.kill().await;
                continue;
            }
        };

        let server = LspServer::new(capabilities);

        let pid = child
            .id()
            .context(format!("{}: no PID after spawn", def.name))?;
        let Some(mut tree_monitor) = catenary_proc::TreeMonitor::new(pid) else {
            println!("  SKIP: could not create TreeMonitor for PID {pid}");
            let _ = child.kill().await;
            continue;
        };

        // Create recording sink.
        let mut sink = RecordingSink::new(&db_path)?;

        // Start settle loop.
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        // The settle loop runs in a spawned task so we can do stimulus
        // work concurrently. All owned data moves into the task.
        let server_name = def.name.to_string();
        let settle_handle = tokio::spawn(async move {
            settle_loop(
                &mut tree_monitor,
                &server,
                &server_name,
                SAMPLE_INTERVAL,
                &mut sink,
                cancel_clone,
            )
            .await;
        });

        // ── Idle baseline ────────────────────────────────────────────
        println!("  idle baseline ({IDLE_DURATION:?})...");
        tokio::time::sleep(IDLE_DURATION).await;

        // ── Stimulus: open bad file ──────────────────────────────────
        let bad_path = work_dir.path().join(def.bad_file);
        let bad_content = std::fs::read_to_string(&bad_path).unwrap_or_default();
        let bad_uri = format!("file://{}", bad_path.display());

        let open_bad = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": bad_uri,
                    "languageId": def.language_id,
                    "version": 1,
                    "text": bad_content
                }
            }
        });
        send_lsp(&mut stdin, &open_bad).await?;
        println!("  stimulus ({STIMULUS_DURATION:?})...");
        tokio::time::sleep(STIMULUS_DURATION).await;

        // Drain any pending messages.
        drain_lsp(&mut reader).await;

        // ── Recovery: open clean file or close bad file ──────────────
        if let Some(clean_file) = def.clean_file {
            let clean_path = work_dir.path().join(clean_file);
            let clean_content = std::fs::read_to_string(&clean_path).unwrap_or_default();
            let clean_uri = format!("file://{}", clean_path.display());

            let open_clean = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": {
                        "uri": clean_uri,
                        "languageId": def.language_id,
                        "version": 1,
                        "text": clean_content
                    }
                }
            });
            send_lsp(&mut stdin, &open_clean).await?;
        } else {
            // Close the bad file to trigger re-analysis.
            let close = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didClose",
                "params": {
                    "textDocument": { "uri": bad_uri }
                }
            });
            send_lsp(&mut stdin, &close).await?;
        }
        println!("  recovery ({RECOVERY_DURATION:?})...");
        tokio::time::sleep(RECOVERY_DURATION).await;

        // ── Teardown ─────────────────────────────────────────────────
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), settle_handle).await;

        drain_lsp(&mut reader).await;
        shutdown_server(&mut stdin, &mut reader, &mut child).await;

        println!("  done.");
        any_server_ran = true;
    }

    if !any_server_ran {
        bail!("No LSP servers found — at least rust-analyzer should be installed");
    }

    // Print summary.
    print_summary(&db_path)?;

    // Print DB path so the user can query it manually.
    println!("Database: {}", db_path.display());

    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Catenary MCP server and CLI.
//!
//! This is the main entry point for the Catenary multiplexing bridge.
//! It can be run as an MCP server or as a CLI tool to list and monitor sessions.

#![allow(clippy::print_stdout, reason = "CLI tool needs to output to stdout")]
#![allow(clippy::print_stderr, reason = "CLI tool needs to output to stderr")]

use anyhow::Result;
use chrono::Utc;
use clap::{Parser, Subcommand};
use regex::Regex;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use catenary_mcp::bridge::{DocumentManager, LspBridgeHandler, PathValidator};
use catenary_mcp::cli::{self, ColorConfig, ColumnWidths};
use catenary_mcp::lsp;
use catenary_mcp::mcp::McpServer;
use catenary_mcp::session::{self, EventKind, Session, SessionEvent};

/// Command-line arguments for Catenary.
#[derive(Parser, Debug)]
#[command(name = "catenary")]
#[command(about = "Multiplexing bridge between MCP and multiple LSP servers")]
struct Args {
    /// The subcommand to run.
    #[command(subcommand)]
    command: Option<Command>,

    /// LSP servers to spawn in "lang:command" format (e.g., "rust:rust-analyzer").
    /// Can be specified multiple times. These override/append to the config file.
    #[arg(short, long = "lsp", global = true)]
    lsps: Vec<String>,

    /// Path to configuration file.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Workspace root directories. Can be specified multiple times.
    #[arg(short, long, global = true)]
    root: Vec<PathBuf>,

    /// Document idle timeout in seconds before auto-close (0 to disable).
    /// Overrides config file if set (default in config is 300).
    #[arg(long, global = true)]
    idle_timeout: Option<u64>,
}

/// Subcommands supported by Catenary.
#[derive(Subcommand, Debug)]
enum Command {
    /// Run the MCP server (default if no subcommand given).
    Serve,

    /// List active Catenary sessions.
    List,

    /// Monitor events from a session.
    Monitor {
        /// Session ID or row number (use 'catenary list' to see available sessions).
        id: String,

        /// Show raw JSON output.
        #[arg(long)]
        raw: bool,

        /// Disable colored output.
        #[arg(long)]
        nocolor: bool,

        /// Filter events by regex pattern.
        #[arg(long, short)]
        filter: Option<String>,
    },

    /// Show status of a session.
    Status {
        /// Session ID (use 'catenary list' to see available sessions).
        id: String,
    },

    /// Notify a running session of a file change (used by `PostToolUse` hooks).
    /// Reads hook JSON from stdin, connects to the session's notify socket,
    /// and prints any LSP diagnostics to stdout.
    Notify {
        /// Output format: "plain" (default) or "gemini".
        #[arg(long, default_value = "plain")]
        format: String,
    },

    /// Check language server health for the current workspace.
    Doctor {
        /// Disable colored output.
        #[arg(long)]
        nocolor: bool,
    },

    /// Sync /add-dir roots from Claude Code transcript to a running session.
    /// Designed for `PreToolUse` hooks — reads hook JSON from stdin.
    SyncRoots {
        /// Output format: "plain" (default) or "gemini".
        #[arg(long, default_value = "plain")]
        format: String,
    },
}

/// Entry point for the Catenary binary.
///
/// # Errors
///
/// Returns an error if the subcommand fails.
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    match args.command {
        None | Some(Command::Serve) => run_server(args).await,
        Some(Command::List) => run_list(),
        Some(Command::Monitor {
            id,
            raw,
            nocolor,
            filter,
        }) => run_monitor(&id, raw, nocolor, filter.as_deref()),
        Some(Command::Status { id }) => run_status(&id),
        Some(Command::Notify { format }) => {
            run_notify(&format);
            Ok(())
        }
        Some(Command::Doctor { nocolor }) => run_doctor(args, nocolor).await,
        Some(Command::SyncRoots { format }) => {
            run_sync_roots(&format);
            Ok(())
        }
    }
}

/// Run the MCP server (main functionality)
/// Runs the MCP server.
///
/// # Errors
///
/// Returns an error if the server fails to start or encounters an internal error.
#[allow(
    clippy::too_many_lines,
    reason = "Server setup requires sequential initialization steps"
)]
async fn run_server(args: Args) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("catenary=info".parse()?))
        .with_writer(std::io::stderr)
        .init();

    // Load configuration
    let mut config = catenary_mcp::config::Config::load(args.config.clone())?;

    // Override idle_timeout if provided on CLI
    if let Some(timeout) = args.idle_timeout {
        config.idle_timeout = timeout;
    }

    // Merge CLI LSPs into config
    for lsp_spec in args.lsps {
        let (lang, command_str) = lsp_spec.split_once(':').ok_or_else(|| {
            anyhow::anyhow!("Invalid LSP spec: {lsp_spec}. Expected 'lang:command'")
        })?;

        let lang = lang.trim().to_string();
        let command_str = command_str.trim();

        // Parse command into program and arguments
        let mut parts = command_str.split_whitespace();
        let program = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("command cannot be empty"))?
            .to_string();
        let cmd_args: Vec<String> = parts.map(std::string::ToString::to_string).collect();

        config.server.insert(
            lang,
            catenary_mcp::config::ServerConfig {
                command: program,
                args: cmd_args,
                initialization_options: None,
            },
        );
    }

    // Default to current directory if no roots specified
    let raw_roots = if args.root.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        args.root
    };
    let roots: Vec<PathBuf> = raw_roots
        .into_iter()
        .map(|r| r.canonicalize())
        .collect::<std::io::Result<Vec<_>>>()?;

    let workspace_display = roots
        .iter()
        .map(|r| r.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", ");

    // Create session for observability
    let session = Arc::new(std::sync::Mutex::new(Session::create(&workspace_display)?));
    let broadcaster = session
        .lock()
        .map_err(|_| anyhow::anyhow!("mutex poisoned"))?
        .broadcaster();

    info!("Starting catenary multiplexing bridge");
    info!(
        "Session ID: {}",
        session
            .lock()
            .map_err(|_| anyhow::anyhow!("mutex poisoned"))?
            .info
            .id
    );
    info!("Workspace roots: {}", workspace_display);
    info!("Document idle timeout: {}s", config.idle_timeout);

    // Create managers
    let client_manager = Arc::new(lsp::ClientManager::new(
        config.clone(),
        roots,
        broadcaster.clone(),
    ));
    client_manager.spawn_all().await;

    let doc_manager = Arc::new(Mutex::new(DocumentManager::new()));
    let runtime = tokio::runtime::Handle::current();

    // Start document cleanup task if timeout is enabled
    let cleanup_handle = if config.idle_timeout > 0 {
        let client_manager_clone = client_manager.clone();
        let doc_manager_clone = doc_manager.clone();
        let idle_timeout = config.idle_timeout;

        Some(tokio::spawn(async move {
            document_cleanup_task(client_manager_clone, doc_manager_clone, idle_timeout).await;
        }))
    } else {
        None
    };

    let current_roots = client_manager.roots().await;

    let path_validator = Arc::new(tokio::sync::RwLock::new(PathValidator::new(
        current_roots.clone(),
    )));

    // Start the notify socket server for PostToolUse hook integration
    let notify_server = catenary_mcp::notify::NotifyServer::new(
        client_manager.clone(),
        doc_manager.clone(),
        path_validator.clone(),
    );
    let socket_path = session
        .lock()
        .map_err(|_| anyhow::anyhow!("mutex poisoned"))?
        .socket_path();
    let notify_handle = notify_server.start(&socket_path)?;
    session
        .lock()
        .map_err(|_| anyhow::anyhow!("mutex poisoned"))?
        .set_socket_active();

    let handler = LspBridgeHandler::new(
        client_manager.clone(),
        doc_manager,
        runtime,
        broadcaster.clone(),
        path_validator.clone(),
    );

    // Run MCP server (blocking - reads from stdin)
    let session_for_callback = session.clone();
    let client_manager_for_roots = client_manager.clone();
    let path_validator_for_roots = path_validator.clone();
    let runtime_for_roots = tokio::runtime::Handle::current();
    let mut mcp_server = McpServer::new(handler, broadcaster)
        .on_client_info(Box::new(move |name: &str, version: &str| {
            if let Ok(mut session) = session_for_callback.lock() {
                session.set_client_info(name, version);
            }
        }))
        .on_roots_changed(Box::new(move |roots| {
            let paths: Vec<PathBuf> = roots
                .iter()
                .filter_map(|root| {
                    root.uri.strip_prefix("file://").and_then(|p| {
                        let path = PathBuf::from(p);
                        match path.canonicalize() {
                            Ok(canonical) => Some(canonical),
                            Err(e) => {
                                warn!("Skipping root {p}: {e}");
                                None
                            }
                        }
                    })
                })
                .collect();

            // Update path validator with new roots
            runtime_for_roots
                .block_on(path_validator_for_roots.write())
                .update_roots(paths.clone());

            runtime_for_roots.block_on(client_manager_for_roots.sync_roots(paths))?;
            runtime_for_roots.block_on(client_manager_for_roots.spawn_all());
            Ok(())
        }));

    // Run in a blocking task since MCP server uses synchronous I/O
    let mcp_task = tokio::task::spawn_blocking(move || mcp_server.run());

    // Wait for either the MCP task to finish or a termination signal
    let mcp_result = tokio::select! {
        res = mcp_task => {
            res?
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received shutdown signal");
            Ok(())
        }
    };

    // Stop notify socket server
    notify_handle.abort();
    let _ = notify_handle.await;

    // Stop cleanup task
    if let Some(handle) = cleanup_handle {
        handle.abort();
        let _ = handle.await;
    }

    // Shutdown LSP clients gracefully
    info!("Shutting down LSP servers");
    client_manager.shutdown_all().await;

    // Session cleanup happens automatically via Drop

    mcp_result
}

/// List all active sessions
/// Runs the session list command.
///
/// # Errors
///
/// Returns an error if listing sessions fails.
fn run_list() -> Result<()> {
    let sessions = session::list_sessions()?;

    if sessions.is_empty() {
        println!("No active Catenary sessions");
        return Ok(());
    }

    let term_width = cli::terminal_width();
    let widths = ColumnWidths::calculate(term_width);

    // Print header
    println!(
        "{:>width_num$} {:<width_id$} {:<width_pid$} {:<width_ws$} {:<width_client$} {:<width_lang$} STARTED",
        "#",
        "ID",
        "PID",
        "WORKSPACE",
        "CLIENT",
        "LANGUAGES",
        width_num = widths.row_num,
        width_id = widths.id,
        width_pid = widths.pid,
        width_ws = widths.workspace,
        width_client = widths.client,
        width_lang = widths.languages,
    );
    println!("{}", "-".repeat(term_width.min(120)));

    for (idx, s) in sessions.iter().enumerate() {
        let client = match (&s.client_name, &s.client_version) {
            (Some(name), Some(ver)) => format!("{name} v{ver}"),
            (Some(name), None) => name.clone(),
            _ => "-".to_string(),
        };

        let ago = format_duration_ago(s.started_at);

        // Get active languages for this session
        let languages = session::active_languages(&s.id)
            .unwrap_or_default()
            .join(",");
        let languages = if languages.is_empty() {
            "-".to_string()
        } else {
            languages
        };

        // Truncate fields to fit column widths
        let id = cli::truncate(&s.id, widths.id);
        let workspace = cli::truncate(&s.workspace, widths.workspace);
        let client = cli::truncate(&client, widths.client);
        let languages = cli::truncate(&languages, widths.languages);

        println!(
            "{:>width_num$} {:<width_id$} {:<width_pid$} {:<width_ws$} {:<width_client$} {:<width_lang$} {}",
            idx + 1,
            id,
            s.pid,
            workspace,
            client,
            languages,
            ago,
            width_num = widths.row_num,
            width_id = widths.id,
            width_pid = widths.pid,
            width_ws = widths.workspace,
            width_client = widths.client,
            width_lang = widths.languages,
        );
    }

    Ok(())
}

/// Resolve a session ID from either a row number or ID prefix
fn resolve_session_id(id: &str) -> Result<session::SessionInfo> {
    // Try parsing as a row number first (1-indexed)
    if let Ok(row_num) = id.parse::<usize>()
        && row_num > 0
    {
        let sessions = session::list_sessions()?;
        if let Some(s) = sessions.get(row_num - 1) {
            return Ok(s.clone());
        }
        // Row number out of range — try as session ID prefix before giving up.
        // Session IDs are hex strings that may be all digits (e.g., "025586387"),
        // so a purely numeric input could be either a row number or a session ID.
        if let Ok(session) = find_session(id) {
            return Ok(session);
        }
        anyhow::bail!("Row number {} out of range (1-{})", row_num, sessions.len());
    }

    // Fall back to find_session (ID prefix matching)
    find_session(id)
}

/// Monitor events from a session
/// Runs the monitor command.
///
/// # Errors
///
/// Returns an error if the session cannot be found or monitoring fails.
fn run_monitor(id: &str, raw: bool, nocolor: bool, filter: Option<&str>) -> Result<()> {
    // Resolve session ID (supports row numbers and prefix matching)
    let session = resolve_session_id(id)?;
    let full_id = session.id;

    let colors = ColorConfig::new(nocolor);
    let term_width = cli::terminal_width();

    // Compile filter regex if provided
    let filter_regex = filter
        .as_ref()
        .map(|f| Regex::new(f))
        .transpose()
        .map_err(|e| anyhow::anyhow!("Invalid filter regex: {e}"))?;

    println!("Monitoring session {full_id} (Ctrl+C to stop)\n");

    let mut reader = session::tail_events(&full_id)?;

    loop {
        if let Some(event) = reader.next_event()? {
            // Apply filter if set
            if let Some(ref re) = filter_regex {
                let event_str = format!("{:?}", event.kind);
                if !re.is_match(&event_str) {
                    continue;
                }
            }

            if raw {
                print_event_raw(&event);
            } else {
                print_event_annotated(&event, &colors, term_width);
            }
        } else {
            println!("\nSession ended");
            break;
        }
    }

    Ok(())
}

/// Show status of a session
/// Runs the status command.
///
/// # Errors
///
/// Returns an error if the session cannot be found.
fn run_status(id: &str) -> Result<()> {
    let session = find_session(id)?;

    println!("Session: {}", session.id);
    println!("PID: {}", session.pid);
    println!("Workspace: {}", session.workspace);
    println!(
        "Started: {} ({})",
        session.started_at.format("%Y-%m-%d %H:%M:%S UTC"),
        format_duration_ago(session.started_at)
    );

    if let Some(name) = &session.client_name {
        print!("Client: {name}");
        if let Some(ver) = &session.client_version {
            print!(" v{ver}");
        }
        println!();
    }

    // Show recent events
    println!("\nRecent events:");
    let events: Vec<_> = session::monitor_events(&session.id)?.collect();
    let recent: Vec<_> = events.iter().rev().take(10).collect();

    for event in recent.iter().rev() {
        print_event(event);
    }

    Ok(())
}

/// Notify a running session of a file change (`PostToolUse` hook handler).
///
/// Reads hook JSON from stdin, finds the matching session by workspace,
/// connects to its notify socket, and prints diagnostics to stdout.
/// Silently succeeds on any error to avoid breaking Claude Code's flow.
fn run_notify(format: &str) {
    use std::io::{BufRead, Write};

    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        return;
    };

    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        return;
    };

    // Extract file_path from tool_input
    let file_path = hook_json
        .get("tool_input")
        .and_then(|ti| ti.get("file_path").or_else(|| ti.get("file")))
        .and_then(|fp| fp.as_str());

    let Some(file_path) = file_path else {
        return;
    };

    // Resolve to absolute path
    let abs_path = if std::path::Path::new(file_path).is_absolute() {
        std::path::PathBuf::from(file_path)
    } else {
        let cwd = std::env::current_dir().unwrap_or_default();
        cwd.join(file_path)
    };

    // Find session whose workspace contains this file
    let sessions = session::list_sessions().unwrap_or_default();
    let session = sessions
        .iter()
        .find(|s| abs_path.to_string_lossy().starts_with(&s.workspace));

    let Some(session) = session else {
        return;
    };

    // Connect to the session's notify socket
    let socket_path = session::sessions_dir()
        .join(&session.id)
        .join("notify.sock");

    if !socket_path.exists() {
        return;
    }

    let Ok(stream) = std::os::unix::net::UnixStream::connect(&socket_path) else {
        return;
    };

    // Set a timeout so we don't hang forever
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    // Send the file path as JSON
    let request = serde_json::json!({ "file": abs_path.to_string_lossy() });
    let mut writer = std::io::BufWriter::new(&stream);
    if serde_json::to_writer(&mut writer, &request).is_err() {
        return;
    }
    if writeln!(writer).is_err() || writer.flush().is_err() {
        return;
    }

    // Read the response (diagnostics)
    let reader = std::io::BufReader::new(&stream);
    let mut lines = Vec::new();
    for line in reader.lines() {
        match line {
            Ok(text) if !text.is_empty() => lines.push(text),
            _ => break,
        }
    }

    if lines.is_empty() {
        return;
    }

    let output = format_diagnostics(&lines, format);
    print!("{output}");
}

/// Sync `/add-dir` roots from Claude Code transcript to a running Catenary session.
///
/// Reads hook JSON from stdin, scans the transcript for `/add-dir` confirmation
/// messages, and sends newly discovered roots to the session's notify socket.
/// Uses a byte-offset cache to avoid re-scanning the entire transcript each time.
///
/// Silently succeeds on any error to avoid breaking Claude Code's flow.
#[allow(
    clippy::too_many_lines,
    reason = "Sequential hook processing with early returns"
)]
fn run_sync_roots(format: &str) {
    use std::io::{BufRead, Seek, SeekFrom, Write};

    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        return;
    };

    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        return;
    };

    // Extract transcript_path and cwd from hook input
    let Some(transcript_path) = hook_json.get("transcript_path").and_then(|v| v.as_str()) else {
        return;
    };

    let cwd = hook_json.get("cwd").and_then(|v| v.as_str()).map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        PathBuf::from,
    );

    // Find the session whose workspace matches cwd
    let sessions = session::list_sessions().unwrap_or_default();
    let cwd_str = cwd.to_string_lossy();
    let session = sessions.iter().find(|s| cwd_str.starts_with(&s.workspace));

    let Some(session) = session else {
        return;
    };

    let session_dir = session::sessions_dir().join(&session.id);

    // Read the byte offset from previous invocation
    let offset_path = session_dir.join("transcript_offset");
    let start_offset: u64 = std::fs::read_to_string(&offset_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    // Open transcript and seek to offset
    let Ok(mut file) = std::fs::File::open(transcript_path) else {
        return;
    };

    if file.seek(SeekFrom::Start(start_offset)).is_err() {
        return;
    }

    // Scan new lines for /add-dir confirmation messages
    let mut new_roots = Vec::new();
    let reader = std::io::BufReader::new(&mut file);

    // Pattern: Added \x1b[1m/path\x1b[22m as a working directory
    // In the JSONL file, ESC (\x1b) is JSON-encoded as \u001b, and we're
    // reading raw text lines (not deserializing), so match the escaped form.
    let add_dir_pattern = "Added \\u001b[1m";
    let add_dir_suffix = "\\u001b[22m as a working directory";

    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };

        // Each line is a JSON object; look inside message.content for the pattern
        if !line.contains(add_dir_pattern) {
            continue;
        }

        // Extract all occurrences of the pattern from this line
        let mut search_from = 0;
        while let Some(start) = line[search_from..].find(add_dir_pattern) {
            let abs_start = search_from + start + add_dir_pattern.len();
            if let Some(end) = line[abs_start..].find(add_dir_suffix) {
                let path_str = &line[abs_start..abs_start + end];
                // Unescape JSON string escapes (the path is inside a JSON string)
                let path_str = path_str
                    .replace("\\\\", "\\")
                    .replace("\\/", "/")
                    .replace("\\\"", "\"");
                let path = PathBuf::from(&path_str);
                let resolved = if path.is_absolute() {
                    path
                } else {
                    cwd.join(path)
                };
                if !new_roots.contains(&resolved) {
                    new_roots.push(resolved);
                }
                search_from = abs_start + end + add_dir_suffix.len();
            } else {
                break;
            }
        }
    }

    // Update the byte offset for next invocation
    if let Ok(pos) = file.stream_position() {
        let _ = std::fs::write(&offset_path, pos.to_string());
    }

    if new_roots.is_empty() {
        return;
    }

    // Connect to the session's notify socket
    let socket_path = session_dir.join("notify.sock");
    if !socket_path.exists() {
        return;
    }

    let Ok(stream) = std::os::unix::net::UnixStream::connect(&socket_path) else {
        return;
    };

    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    // Send the add_roots request
    let root_strings: Vec<String> = new_roots
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let request = serde_json::json!({ "add_roots": root_strings });
    let mut writer = std::io::BufWriter::new(&stream);
    if serde_json::to_writer(&mut writer, &request).is_err() {
        return;
    }
    if writeln!(writer).is_err() || writer.flush().is_err() {
        return;
    }

    // Read the response
    let reader = std::io::BufReader::new(&stream);
    let mut lines = Vec::new();
    for line in reader.lines() {
        match line {
            Ok(text) if !text.is_empty() => lines.push(text),
            _ => break,
        }
    }

    if lines.is_empty() {
        return;
    }

    let output = format_diagnostics(&lines, format);
    print!("{output}");
}

/// Format diagnostic lines for output.
///
/// - `"gemini"`: wraps in a JSON envelope for Gemini CLI hooks.
/// - Any other value (including `"plain"`): joins lines with newlines and a trailing newline.
fn format_diagnostics(lines: &[String], format: &str) -> String {
    if format == "gemini" {
        let diagnostics = lines.join("\n");
        let envelope = serde_json::json!({
            "hookSpecificOutput": {
                "additionalContext": format!("LSP Diagnostics:\n{diagnostics}")
            }
        });
        // serde_json::to_string cannot fail on Value
        envelope.to_string()
    } else {
        let mut out = lines.join("\n");
        out.push('\n');
        out
    }
}

/// Run the doctor command: check language server health for the current workspace.
///
/// # Errors
///
/// Returns an error if the configuration cannot be loaded or roots cannot be resolved.
#[allow(
    clippy::too_many_lines,
    reason = "Doctor command has sequential output logic"
)]
async fn run_doctor(args: Args, nocolor: bool) -> Result<()> {
    let colors = ColorConfig::new(nocolor);

    // Load configuration (same as run_server)
    let mut config = catenary_mcp::config::Config::load(args.config.clone())?;
    for lsp_spec in &args.lsps {
        let (lang, command_str) = lsp_spec.split_once(':').ok_or_else(|| {
            anyhow::anyhow!("Invalid LSP spec: {lsp_spec}. Expected 'lang:command'")
        })?;
        let lang = lang.trim().to_string();
        let command_str = command_str.trim();
        let mut parts = command_str.split_whitespace();
        let program = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("command cannot be empty"))?
            .to_string();
        let cmd_args: Vec<String> = parts.map(std::string::ToString::to_string).collect();
        config.server.insert(
            lang,
            catenary_mcp::config::ServerConfig {
                command: program,
                args: cmd_args,
                initialization_options: None,
            },
        );
    }

    // Resolve workspace roots
    let raw_roots = if args.root.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        args.root
    };
    let roots: Vec<PathBuf> = raw_roots
        .into_iter()
        .map(|r| r.canonicalize())
        .collect::<std::io::Result<Vec<_>>>()?;

    // Print config and roots
    let config_source = args
        .config
        .as_ref()
        .map_or_else(|| "default paths".to_string(), |p| p.display().to_string());
    println!("{} {}", colors.bold("Config:"), config_source);
    println!(
        "{} {}",
        colors.bold("Roots: "),
        roots
            .iter()
            .map(|r| r.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!();

    if config.server.is_empty() {
        println!("No language servers configured.");
        return Ok(());
    }

    // Detect which languages have files in the workspace
    let configured_keys: std::collections::HashSet<&str> =
        config.server.keys().map(String::as_str).collect();
    let detected = lsp::detect_workspace_languages(&roots, &configured_keys);

    // Sort servers alphabetically
    let mut servers: Vec<(&String, &catenary_mcp::config::ServerConfig)> =
        config.server.iter().collect();
    servers.sort_by_key(|(lang, _)| *lang);

    // Determine column width for language name
    let max_lang_width = servers.iter().map(|(l, _)| l.len()).max().unwrap_or(10);
    let max_cmd_width = servers
        .iter()
        .map(|(_, s)| s.command.len())
        .max()
        .unwrap_or(10);

    // Create a broadcaster for client spawning (no-op since we don't need events)
    let broadcaster = catenary_mcp::session::EventBroadcaster::noop()?;

    for (lang, server_config) in &servers {
        let lang_display = format!("{lang:<max_lang_width$}");
        let cmd_display = format!("{cmd:<max_cmd_width$}", cmd = server_config.command);

        // Check if any files for this language exist
        if !detected.contains(lang.as_str()) {
            println!(
                "{}  {}  {}",
                colors.dim(&lang_display),
                colors.dim(&cmd_display),
                colors.dim("- skipped (no matching files)"),
            );
            continue;
        }

        // Check if binary exists on PATH
        if !binary_exists(&server_config.command) {
            println!(
                "{}  {}  {}",
                lang_display,
                cmd_display,
                colors.red("✗ command not found"),
            );
            continue;
        }

        // Spawn and initialize the server
        let args_refs: Vec<&str> = server_config.args.iter().map(String::as_str).collect();
        let spawn_result = lsp::LspClient::spawn_quiet(
            &server_config.command,
            &args_refs,
            lang,
            broadcaster.clone(),
        );

        let mut client = match spawn_result {
            Ok(client) => client,
            Err(e) => {
                println!(
                    "{}  {}  {}",
                    lang_display,
                    cmd_display,
                    colors.red(&format!("✗ spawn failed: {e}")),
                );
                continue;
            }
        };

        match client
            .initialize(&roots, server_config.initialization_options.clone())
            .await
        {
            Ok(result) => {
                let tools = extract_capabilities(&result.capabilities);
                println!(
                    "{}  {}  {}",
                    lang_display,
                    cmd_display,
                    colors.green("✓ ready"),
                );
                if !tools.is_empty() {
                    println!(
                        "{}  {}",
                        " ".repeat(max_lang_width + max_cmd_width + 4),
                        colors.dim(&tools.join(" ")),
                    );
                }
            }
            Err(e) => {
                println!(
                    "{}  {}  {}",
                    lang_display,
                    cmd_display,
                    colors.red(&format!("✗ initialize failed: {e}")),
                );
            }
        }

        // Shutdown cleanly
        let _ = client.shutdown().await;
    }

    Ok(())
}

/// Checks whether a binary can be found on `$PATH`.
fn binary_exists(command: &str) -> bool {
    // If the command contains a path separator, check it directly
    if command.contains('/') {
        return std::path::Path::new(command).exists();
    }

    // Search PATH
    let path_var = std::env::var("PATH").unwrap_or_default();
    std::env::split_paths(&path_var).any(|dir| dir.join(command).is_file())
}

/// Extracts Catenary tool names from LSP `ServerCapabilities`.
fn extract_capabilities(caps: &lsp_types::ServerCapabilities) -> Vec<&'static str> {
    let mut tools = Vec::new();

    if caps.hover_provider.is_some() {
        tools.push("hover");
    }
    if caps.definition_provider.is_some() {
        tools.push("definition");
    }
    if caps.type_definition_provider.is_some() {
        tools.push("type_definition");
    }
    if caps.implementation_provider.is_some() {
        tools.push("implementation");
    }
    if caps.references_provider.is_some() {
        tools.push("references");
    }
    if caps.document_symbol_provider.is_some() {
        tools.push("document_symbols");
    }
    if caps.workspace_symbol_provider.is_some() {
        tools.push("search");
    }
    if caps.code_action_provider.is_some() {
        tools.push("code_actions");
    }
    if caps.rename_provider.is_some() {
        tools.push("rename");
    }
    if caps.call_hierarchy_provider.is_some() {
        tools.push("call_hierarchy");
    }
    // type_hierarchy_provider is not exposed as a direct field in lsp_types 0.97;
    // type hierarchy support is probed at call time, so we omit it here.

    tools
}

/// Find session by ID or prefix
fn find_session(id: &str) -> Result<session::SessionInfo> {
    // Try exact match first
    if let Some(s) = session::get_session(id)? {
        return Ok(s);
    }

    // Try prefix match
    let sessions = session::list_sessions()?;
    let matches: Vec<_> = sessions.iter().filter(|s| s.id.starts_with(id)).collect();

    match matches.len() {
        0 => anyhow::bail!("No session found matching '{id}'"),
        1 => Ok(matches[0].clone()),
        _ => {
            eprintln!("Multiple sessions match '{id}':");
            for s in matches {
                eprintln!("  {}", s.id);
            }
            anyhow::bail!("Please specify a more complete session ID")
        }
    }
}

/// Format a timestamp as "Xm ago" or similar
fn format_duration_ago(timestamp: chrono::DateTime<Utc>) -> String {
    let now = Utc::now();
    let duration = now.signed_duration_since(timestamp);

    if duration.num_hours() > 0 {
        format!(
            "{}h {}m ago",
            duration.num_hours(),
            duration.num_minutes() % 60
        )
    } else if duration.num_minutes() > 0 {
        format!("{}m ago", duration.num_minutes())
    } else {
        format!("{}s ago", duration.num_seconds())
    }
}

/// Print an event in raw JSON format
fn print_event_raw(event: &SessionEvent) {
    let time = event.timestamp.format("%H:%M:%S");

    if let EventKind::McpMessage { direction, message } = &event.kind {
        let arrow = if direction == "in" { "→" } else { "←" };
        println!("[{time}] {arrow}");
        let pretty = serde_json::to_string_pretty(message).unwrap_or_default();
        println!("{pretty}");
    } else {
        // For non-MCP events, print as JSON
        let json = serde_json::to_string_pretty(&event.kind).unwrap_or_default();
        println!("[{time}] {json}");
    }
}

/// Print an event with annotations and colors
fn print_event_annotated(event: &SessionEvent, colors: &ColorConfig, term_width: usize) {
    let time = event.timestamp.format("%H:%M:%S");
    let time_str = colors.dim(&format!("[{time}]"));

    match &event.kind {
        EventKind::Started => {
            println!("{time_str} Session started");
        }
        EventKind::Shutdown => {
            println!("{time_str} Session shutting down");
        }
        EventKind::ServerState { language, state } => {
            let lang = colors.cyan(language);
            println!("{time_str} {lang}: {state}");
        }
        EventKind::Progress {
            language,
            title,
            message,
            percentage,
        } => {
            let lang = colors.cyan(language);
            let pct = percentage.map(|p| format!(" {p}%")).unwrap_or_default();
            let msg = message
                .as_ref()
                .map(|m| format!(" ({m})"))
                .unwrap_or_default();
            println!("{time_str} {lang}: {title}{pct}{msg}");
        }
        EventKind::ProgressEnd { language } => {
            let lang = colors.cyan(language);
            println!("{time_str} {lang}: Ready");
        }
        EventKind::ToolCall { tool, file } => {
            let arrow = colors.green("→");
            let file_str = file
                .as_ref()
                .map(|f| format!(" on {f}"))
                .unwrap_or_default();
            println!("{time_str} {arrow} {tool}{file_str}");
        }
        EventKind::ToolResult {
            tool,
            success,
            duration_ms,
        } => {
            let arrow = colors.blue("←");
            let status = if *success {
                "ok".to_string()
            } else {
                colors.red("error")
            };
            println!("{time_str} {arrow} {tool} -> {status} ({duration_ms}ms)");
        }
        EventKind::McpMessage { direction, message } => {
            let arrow_colored = if direction == "in" {
                colors.green("→")
            } else {
                colors.blue("←")
            };

            // Extract meaningful info from MCP message
            let summary = extract_mcp_summary(message, colors);

            // Calculate available width for message
            // Format: [HH:MM:SS] → summary
            let prefix_len = 10 + 2 + 2; // [time] + arrow + spaces
            let max_summary_len = term_width.saturating_sub(prefix_len);

            let summary = cli::truncate(&summary, max_summary_len);
            println!("{time_str} {arrow_colored} {summary}");

            // Check for errors in response
            if direction == "out"
                && let Some(obj) = message.as_object()
                && obj.contains_key("error")
                && let Some(error) = obj.get("error")
            {
                let err_msg = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown error");
                println!("    {}", colors.red(&format!("Error: {err_msg}")));
            }
        }
    }
}

/// Extract a human-readable summary from an MCP message
fn extract_mcp_summary(message: &serde_json::Value, colors: &ColorConfig) -> String {
    let Some(obj) = message.as_object() else {
        return message.to_string();
    };

    // Check if this is a request (has method)
    obj.get("method").and_then(|m| m.as_str()).map_or_else(
        || {
            // Check if this is a response (has result or error)
            if obj.contains_key("result") || obj.contains_key("error") {
                let id = obj.get("id").map(|i| format!("#{i}")).unwrap_or_default();

                if obj.contains_key("error") {
                    format!("{} {}", colors.red("error"), id)
                } else {
                    format!("result {id}")
                }
            } else {
                // Fallback: show compact JSON
                serde_json::to_string(message).unwrap_or_default()
            }
        },
        |method| {
            let id = obj.get("id").map(|i| format!("#{i}")).unwrap_or_default();

            // Extract params summary based on method
            let params_summary = match method {
                "tools/call" => {
                    if let Some(params) = obj.get("params")
                        && let Some(name) = params.get("name").and_then(|n| n.as_str())
                    {
                        // Try to get file argument if present
                        let file_info = params
                            .get("arguments")
                            .and_then(|a| a.get("file_path").or_else(|| a.get("path")))
                            .and_then(|f| f.as_str())
                            .map(|f| {
                                // Just show filename, not full path
                                std::path::Path::new(f)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or(f)
                            })
                            .map(|f| format!(" ({f})"))
                            .unwrap_or_default();
                        format!("{}{}", colors.cyan(name), file_info)
                    } else {
                        String::new()
                    }
                }
                "initialize" => {
                    if let Some(params) = obj.get("params")
                        && let Some(info) = params.get("clientInfo")
                        && let Some(name) = info.get("name").and_then(|n| n.as_str())
                    {
                        format!("from {name}")
                    } else {
                        String::new()
                    }
                }
                _ => String::new(),
            };

            if params_summary.is_empty() {
                format!("{method} {id}")
            } else {
                format!("{method} {params_summary} {id}")
            }
        },
    )
}

/// Print an event in human-readable format (used by `run_status`)
fn print_event(event: &SessionEvent) {
    let colors = ColorConfig::new(false);
    let term_width = cli::terminal_width();
    print_event_annotated(event, &colors, term_width);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_diagnostics_plain() {
        let lines = vec![
            "error[E0308]: mismatched types".into(),
            "  --> src/main.rs:5:10".into(),
        ];
        let output = format_diagnostics(&lines, "plain");
        assert_eq!(
            output,
            "error[E0308]: mismatched types\n  --> src/main.rs:5:10\n"
        );
    }

    #[test]
    fn test_format_diagnostics_gemini() {
        let lines = vec!["error[E0308]: mismatched types".into()];
        let output = format_diagnostics(&lines, "gemini");
        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("gemini format should produce valid JSON");

        let context = parsed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("additionalContext should be a string");
        assert!(context.starts_with("LSP Diagnostics:\n"));
        assert!(context.contains("error[E0308]: mismatched types"));
    }

    #[test]
    fn test_format_diagnostics_gemini_multiline() {
        let lines = vec!["warning: unused variable".into(), "  --> lib.rs:3:9".into()];
        let output = format_diagnostics(&lines, "gemini");
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let context = parsed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(context.contains("warning: unused variable\n  --> lib.rs:3:9"));
    }
}

/// Background task that periodically closes idle documents.
async fn document_cleanup_task(
    client_manager: Arc<lsp::ClientManager>,
    doc_manager: Arc<Mutex<DocumentManager>>,
    idle_timeout_secs: u64,
) {
    // Check every 60 seconds or half the timeout, whichever is smaller
    let check_interval = Duration::from_secs(idle_timeout_secs.min(60));

    loop {
        tokio::time::sleep(check_interval).await;

        // Find and close stale documents
        let stale_paths = {
            let doc_manager = doc_manager.lock().await;
            doc_manager.stale_documents(idle_timeout_secs)
        };

        if !stale_paths.is_empty() {
            debug!("Closing {} stale documents", stale_paths.len());

            for path in stale_paths {
                let (lang, close_params) = {
                    let mut doc_manager = doc_manager.lock().await;
                    let lang = doc_manager.language_id_for_path(&path).to_string();
                    (lang, doc_manager.close(&path))
                };

                if let Ok(Some(params)) = close_params {
                    // Only try to close if the client is active
                    let active_clients = client_manager.active_clients().await;
                    if let Some(client_mutex) = active_clients.get(&lang) {
                        let client = client_mutex.lock().await;
                        if let Err(e) = client.did_close(params).await {
                            warn!("Failed to close document {}: {}", path.display(), e);
                        } else {
                            debug!("Closed stale document: {}", path.display());
                        }
                    }
                }
            }
        }

        // Check for idle servers (no open documents) and shut them down
        let active_langs: Vec<String> = client_manager
            .active_clients()
            .await
            .keys()
            .cloned()
            .collect();
        for lang in active_langs {
            let has_docs = {
                let doc_manager = doc_manager.lock().await;
                doc_manager.has_open_documents(&lang)
            };

            if !has_docs {
                // No open documents for this language? Shut it down.
                // Note: This might be aggressive if the user just closed the last file
                // and intends to open another one soon.
                // But since we check on `idle_timeout` interval (e.g. 60s), it's probably fine.
                // Ideally we'd track "server idle time" separately, but this is a good start.
                client_manager.shutdown_client(&lang).await;
            }
        }
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Catenary MCP server and CLI.
//!
//! This is the main entry point for the Catenary multiplexing bridge.
//! It can be run as an MCP server or as a CLI tool to list and monitor sessions.

#![allow(clippy::print_stdout, reason = "CLI tool needs to output to stdout")]
#![allow(clippy::print_stderr, reason = "CLI tool needs to output to stderr")]

use anyhow::Result;
use chrono::{Local, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use regex::Regex;
use std::path::{Path, PathBuf};
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

/// Output format for hook commands.
///
/// Determines how hook output is structured for the host CLI.
/// Required on all hook-facing subcommands (`notify`, `sync-roots`, `lock`).
#[derive(Clone, Copy, Debug, ValueEnum)]
enum HostFormat {
    /// Claude Code hooks (`PostToolUse` / `PreToolUse`).
    Claude,
    /// Gemini CLI hooks (`AfterTool` / `BeforeTool`).
    Gemini,
}

/// Command-line arguments for Catenary.
#[derive(Parser, Debug)]
#[command(name = "catenary")]
#[command(about = "Multiplexing bridge between MCP and multiple LSP servers")]
#[command(version = env!("CATENARY_VERSION"))]
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
        /// Output format: "claude" or "gemini".
        #[arg(long, value_enum)]
        format: HostFormat,
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
        /// Output format: "claude" or "gemini".
        #[arg(long, value_enum)]
        format: HostFormat,
    },

    /// Manage file locks for concurrent agent coordination.
    /// Used by `PreToolUse` and `PostToolUse` hooks to serialize file edits
    /// across multiple agents.
    Lock {
        /// The lock action to perform.
        #[command(subcommand)]
        action: LockAction,
    },
}

/// Lock subcommands for concurrent agent coordination.
#[derive(Subcommand, Clone, Copy, Debug)]
enum LockAction {
    /// Acquire a lock before editing a file.
    /// Blocks until the lock is available or the timeout expires.
    /// Reads hook JSON from stdin.
    Acquire {
        /// Maximum time to wait for the lock (seconds).
        #[arg(long, default_value = "180")]
        timeout: u64,

        /// Output format: "claude" or "gemini".
        #[arg(long, value_enum)]
        format: HostFormat,
    },

    /// Release a lock after editing a file.
    /// Sets a grace period before the lock becomes available to other agents.
    /// Reads hook JSON from stdin.
    Release {
        /// Grace period before the lock expires (seconds).
        #[arg(long, default_value = "30")]
        grace: u64,
    },

    /// Track a file read for change detection.
    /// Records the file's modification time so future lock acquisitions
    /// can warn if the file changed.
    /// Reads hook JSON from stdin.
    TrackRead,
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
            run_notify(format);
            Ok(())
        }
        Some(Command::Doctor { nocolor }) => run_doctor(args, nocolor).await,
        Some(Command::SyncRoots { format }) => {
            run_sync_roots(format);
            Ok(())
        }
        Some(Command::Lock { action }) => {
            run_lock(action);
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
        broadcaster.clone(),
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

    // Track last progress (language, title) for line collapsing.
    // When consecutive progress events share the same title, the monitor
    // overwrites the previous line instead of scrolling.
    let mut last_progress: Option<(String, String)> = None;

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
                // Collapse consecutive progress lines with the same title
                if let EventKind::Progress {
                    ref language,
                    ref title,
                    ..
                } = event.kind
                {
                    let key = (language.clone(), title.clone());
                    if last_progress.as_ref() == Some(&key) {
                        // Same progress context — erase previous line
                        print!("\x1b[A\x1b[2K");
                    }
                    last_progress = Some(key);
                } else {
                    last_progress = None;
                }
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
        session
            .started_at
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S"),
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

/// Returns the IPC endpoint path for a session.
///
/// On Unix this is the Unix socket path in the session directory.
/// On Windows this is a named pipe in the kernel namespace.
fn notify_endpoint(session_id: &str) -> PathBuf {
    #[cfg(unix)]
    {
        session::sessions_dir().join(session_id).join("notify.sock")
    }
    #[cfg(windows)]
    {
        PathBuf::from(format!(r"\\.\pipe\catenary-{session_id}"))
    }
}

/// Connects to a notify IPC endpoint and returns a stream for I/O.
///
/// Returns `None` silently on failure (hooks must not break Claude Code's flow).
#[cfg(unix)]
fn notify_connect(endpoint: &std::path::Path) -> Option<std::os::unix::net::UnixStream> {
    if !endpoint.exists() {
        return None;
    }
    let stream = std::os::unix::net::UnixStream::connect(endpoint).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    Some(stream)
}

/// Connects to a notify IPC endpoint and returns a stream for I/O.
///
/// Returns `None` silently on failure (hooks must not break Claude Code's flow).
#[cfg(windows)]
fn notify_connect(endpoint: &std::path::Path) -> Option<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    // SECURITY_IDENTIFICATION (0x0001_0000) prevents impersonation attacks
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .security_qos_flags(0x0001_0000)
        .open(endpoint)
        .ok()
}

/// Sends a JSON request over an IPC stream and reads response lines.
fn ipc_exchange(
    mut stream: impl std::io::Read + std::io::Write,
    request: &serde_json::Value,
) -> Vec<String> {
    use std::io::BufRead;

    if serde_json::to_writer(&mut stream, request).is_err() {
        return Vec::new();
    }
    if stream.write_all(b"\n").is_err() || stream.flush().is_err() {
        return Vec::new();
    }

    let reader = std::io::BufReader::new(stream);
    let mut lines = Vec::new();
    for line in reader.lines() {
        match line {
            Ok(text) if !text.is_empty() => lines.push(text),
            _ => break,
        }
    }
    lines
}

/// Notify a running session of a file change (`PostToolUse` hook handler).
///
/// Reads hook JSON from stdin, finds the matching session by workspace,
/// connects to its notify endpoint, and prints diagnostics to stdout.
/// Silently succeeds on any error to avoid breaking Claude Code's flow.
fn run_notify(format: HostFormat) {
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

    // Resolve to absolute path using cwd from hook JSON (matching run_sync_roots)
    let abs_path = if std::path::Path::new(file_path).is_absolute() {
        std::path::PathBuf::from(file_path)
    } else {
        let cwd = hook_json.get("cwd").and_then(|v| v.as_str()).map_or_else(
            || std::env::current_dir().unwrap_or_default(),
            PathBuf::from,
        );
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

    let endpoint = notify_endpoint(&session.id);
    let Some(stream) = notify_connect(&endpoint) else {
        return;
    };

    let request = serde_json::json!({ "file": abs_path.to_string_lossy() });
    let lines = ipc_exchange(stream, &request);

    if lines.is_empty() {
        return;
    }

    let output = format_diagnostics(&lines, format, "PostToolUse");
    print!("{output}");
}

/// Sync workspace roots from Claude Code transcript to a running Catenary session.
///
/// Reads hook JSON from stdin, scans the transcript for `/add-dir` additions
/// and directory removal messages, and sends the full root set to the session's
/// notify endpoint. The server diffs against its current state, handling both
/// additions and removals.
///
/// Uses a persistent state file (`known_roots.json`) to track the byte offset
/// and the full discovered root set across invocations.
///
/// Silently succeeds on any error to avoid breaking Claude Code's flow.
#[allow(
    clippy::too_many_lines,
    reason = "Sequential hook processing with early returns"
)]
fn run_sync_roots(format: HostFormat) {
    use std::io::{BufRead, Seek, SeekFrom};

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

    // Load persistent state: byte offset + known root set
    let state_path = session_dir.join("known_roots.json");
    let (start_offset, mut known_roots) = load_root_state(&state_path);

    // Migrate from old transcript_offset file if known_roots.json doesn't exist
    if start_offset == 0 && known_roots.is_empty() {
        let legacy_path = session_dir.join("transcript_offset");
        if legacy_path.exists() {
            // Legacy file only stored offset; remove it and re-scan from
            // beginning to build the full root set.
            let _ = std::fs::remove_file(&legacy_path);
        }
    }

    // Open transcript and seek to offset
    let Ok(mut file) = std::fs::File::open(transcript_path) else {
        return;
    };

    if file.seek(SeekFrom::Start(start_offset)).is_err() {
        return;
    }

    // Transcript patterns (raw JSON-escaped forms):
    // Add:    Added \u001b[1m/path\u001b[22m as a working directory
    // Remove: Removed directory \u001b[1m/path\u001b[22m from workspace
    let add_prefix = "Added \\u001b[1m";
    let add_suffix = "\\u001b[22m as a working directory";
    let remove_prefix = "Removed directory \\u001b[1m";
    let remove_suffix = "\\u001b[22m from workspace";

    let mut changed = false;
    let reader = std::io::BufReader::new(&mut file);

    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };

        // Scan for additions
        if line.contains(add_prefix) {
            let mut search_from = 0;
            while let Some(start) = line[search_from..].find(add_prefix) {
                let abs_start = search_from + start + add_prefix.len();
                if let Some(end) = line[abs_start..].find(add_suffix) {
                    let path_str = unescape_json_path(&line[abs_start..abs_start + end]);
                    let resolved = resolve_transcript_path(&path_str, &cwd);
                    if !known_roots.contains(&resolved) {
                        known_roots.push(resolved);
                        changed = true;
                    }
                    search_from = abs_start + end + add_suffix.len();
                } else {
                    break;
                }
            }
        }

        // Scan for removals
        if line.contains(remove_prefix) {
            let mut search_from = 0;
            while let Some(start) = line[search_from..].find(remove_prefix) {
                let abs_start = search_from + start + remove_prefix.len();
                if let Some(end) = line[abs_start..].find(remove_suffix) {
                    let path_str = unescape_json_path(&line[abs_start..abs_start + end]);
                    let resolved = resolve_transcript_path(&path_str, &cwd);
                    if let Some(pos) = known_roots.iter().position(|r| r == &resolved) {
                        known_roots.remove(pos);
                        changed = true;
                    }
                    search_from = abs_start + end + remove_suffix.len();
                } else {
                    break;
                }
            }
        }
    }

    // Save updated state
    let new_offset = file.stream_position().unwrap_or(start_offset);
    save_root_state(&state_path, new_offset, &known_roots);

    if !changed {
        return;
    }

    // Build the full root set: cwd is always present
    let mut full_roots = vec![cwd];
    for root in &known_roots {
        if !full_roots.contains(root) {
            full_roots.push(root.clone());
        }
    }

    let endpoint = notify_endpoint(&session.id);
    let Some(stream) = notify_connect(&endpoint) else {
        return;
    };

    let root_strings: Vec<String> = full_roots
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let request = serde_json::json!({ "sync_roots": root_strings });
    let lines = ipc_exchange(stream, &request);

    if lines.is_empty() {
        return;
    }

    let output = format_diagnostics(&lines, format, "PreToolUse");
    print!("{output}");
}

/// Unescape JSON string escapes from a transcript path.
fn unescape_json_path(raw: &str) -> String {
    raw.replace("\\\\", "\\")
        .replace("\\/", "/")
        .replace("\\\"", "\"")
}

/// Resolve a transcript path to an absolute path.
fn resolve_transcript_path(path_str: &str, cwd: &Path) -> PathBuf {
    let path = PathBuf::from(path_str);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

/// Persistent state for root tracking across `sync-roots` invocations.
#[derive(serde::Serialize, serde::Deserialize)]
struct RootState {
    /// Byte offset into the transcript file.
    offset: u64,
    /// Known workspace roots (absolute paths).
    roots: Vec<String>,
}

/// Loads the root state from a JSON file. Returns `(0, vec![])` on any error.
fn load_root_state(path: &Path) -> (u64, Vec<PathBuf>) {
    let Ok(data) = std::fs::read_to_string(path) else {
        return (0, Vec::new());
    };
    let Ok(state) = serde_json::from_str::<RootState>(&data) else {
        return (0, Vec::new());
    };
    let roots = state.roots.into_iter().map(PathBuf::from).collect();
    (state.offset, roots)
}

/// Saves the root state to a JSON file. Silently ignores errors.
fn save_root_state(path: &Path, offset: u64, roots: &[PathBuf]) {
    let state = RootState {
        offset,
        roots: roots
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
    };
    if let Ok(json) = serde_json::to_string(&state) {
        let _ = std::fs::write(path, json);
    }
}

/// Dispatch lock subcommands.
///
/// Reads hook JSON from stdin, extracts owner identity and file path,
/// and performs the requested lock operation. Silently succeeds on any
/// error to avoid breaking the host CLI's flow.
fn run_lock(action: LockAction) {
    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        return;
    };

    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        return;
    };

    let owner = extract_owner(&hook_json);
    let Some(file_path) = extract_file_path(&hook_json) else {
        return;
    };

    let Ok(mgr) = catenary_mcp::lock::FileLockManager::new() else {
        return;
    };

    match action {
        LockAction::Acquire { timeout, format } => {
            run_lock_acquire(&mgr, &file_path, &owner, timeout, format, &hook_json);
        }
        LockAction::Release { grace } => {
            run_lock_release(&mgr, &file_path, &owner, grace, &hook_json);
        }
        LockAction::TrackRead => {
            run_lock_track_read(&mgr, &file_path, &owner);
        }
    }
}

/// Acquires a file lock, blocking until available or timeout.
fn run_lock_acquire(
    mgr: &catenary_mcp::lock::FileLockManager,
    file_path: &str,
    owner: &str,
    timeout: u64,
    format: HostFormat,
    hook_json: &serde_json::Value,
) {
    use catenary_mcp::lock::AcquireResult;

    let result = mgr.acquire(file_path, owner, timeout);

    // Broadcast event to monitor (best-effort)
    match &result {
        AcquireResult::Acquired | AcquireResult::AcquiredStaleRead { .. } => {
            broadcast_lock_event(
                hook_json,
                EventKind::LockAcquired {
                    file: file_path.to_string(),
                    owner: owner.to_string(),
                },
            );
        }
        AcquireResult::Denied { .. } => {
            // Read the lock to find who's holding it
            let held_by = "unknown".to_string();
            broadcast_lock_event(
                hook_json,
                EventKind::LockDenied {
                    file: file_path.to_string(),
                    owner: owner.to_string(),
                    held_by,
                },
            );
        }
    }

    match result {
        AcquireResult::Acquired => {
            // Silent success
        }
        AcquireResult::AcquiredStaleRead { context } => {
            let output = format_lock_output(format, Some(&context), None);
            print!("{output}");
        }
        AcquireResult::Denied { reason } => {
            let output = format_lock_output(format, None, Some(&reason));
            print!("{output}");
        }
    }
}

/// Releases a file lock with an optional grace period.
fn run_lock_release(
    mgr: &catenary_mcp::lock::FileLockManager,
    file_path: &str,
    owner: &str,
    grace: u64,
    hook_json: &serde_json::Value,
) {
    if mgr.release(file_path, owner, grace).is_ok() {
        broadcast_lock_event(
            hook_json,
            EventKind::LockReleased {
                file: file_path.to_string(),
                owner: owner.to_string(),
            },
        );
    }
}

/// Records a file read for change detection.
fn run_lock_track_read(mgr: &catenary_mcp::lock::FileLockManager, file_path: &str, owner: &str) {
    let _ = mgr.track_read(file_path, owner);
}

/// Extracts the owner identity from hook JSON.
///
/// Uses `session_id` as the primary key. If `agent_id` is present,
/// appends it as `session_id:agent_id`.
fn extract_owner(hook_json: &serde_json::Value) -> String {
    let session_id = hook_json
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let agent_id = hook_json.get("agent_id").and_then(|v| v.as_str());

    agent_id.map_or_else(
        || session_id.to_string(),
        |aid| format!("{session_id}:{aid}"),
    )
}

/// Extracts the file path from hook JSON's `tool_input`.
fn extract_file_path(hook_json: &serde_json::Value) -> Option<String> {
    let file_path = hook_json
        .get("tool_input")
        .and_then(|ti| ti.get("file_path").or_else(|| ti.get("file")))
        .and_then(|fp| fp.as_str())?;

    // Resolve to absolute path
    let abs_path = if std::path::Path::new(file_path).is_absolute() {
        PathBuf::from(file_path)
    } else {
        let cwd = hook_json.get("cwd").and_then(|v| v.as_str()).map_or_else(
            || std::env::current_dir().unwrap_or_default(),
            PathBuf::from,
        );
        cwd.join(file_path)
    };

    Some(abs_path.to_string_lossy().into_owned())
}

/// Formats lock output for the hook response.
///
/// - `additional_context`: injected when the lock was acquired but the file
///   was modified since the owner's last read.
/// - `deny_reason`: injected when the lock acquisition timed out.
fn format_lock_output(
    format: HostFormat,
    additional_context: Option<&str>,
    deny_reason: Option<&str>,
) -> String {
    let is_gemini = matches!(format, HostFormat::Gemini);

    match (deny_reason, additional_context) {
        // Gemini BeforeTool uses top-level decision/reason
        (Some(reason), _) if is_gemini => serde_json::json!({
            "decision": "deny",
            "reason": reason
        })
        .to_string(),
        // Claude Code PreToolUse uses hookSpecificOutput
        (Some(reason), _) => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason
            }
        })
        .to_string(),
        // Gemini: deny stale reads too — force re-read before editing
        (None, Some(context)) if is_gemini => serde_json::json!({
            "decision": "deny",
            "reason": context
        })
        .to_string(),
        // Claude Code: allow with advisory context
        (None, Some(context)) => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow",
                "additionalContext": context
            }
        })
        .to_string(),
        (None, None) => String::new(),
    }
}

/// Broadcasts a lock event to the monitor (best-effort).
///
/// Finds the Catenary session matching the hook's `cwd` and sends the event
/// via the session's event broadcaster. Silently does nothing if no session
/// is found or the broadcast fails.
fn broadcast_lock_event(hook_json: &serde_json::Value, event: EventKind) {
    use std::io::Write;

    let cwd = hook_json.get("cwd").and_then(|v| v.as_str()).unwrap_or("");

    let sessions = session::list_sessions().unwrap_or_default();
    let Some(session_info) = sessions.iter().find(|s| cwd.starts_with(&s.workspace)) else {
        return;
    };

    // Write directly to the session's events file
    let events_path = session::sessions_dir()
        .join(&session_info.id)
        .join("events.jsonl");

    let event = SessionEvent {
        timestamp: chrono::Utc::now(),
        kind: event,
    };

    if let Ok(mut line) = serde_json::to_string(&event) {
        line.push('\n');
        // Append to events file (best-effort)
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&events_path)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

/// Format diagnostic lines for output.
///
/// Both formats wrap diagnostics in a `hookSpecificOutput` JSON envelope
/// so the host CLI can inject them into the model's context:
///
/// - Gemini: uses `additionalContext` for Gemini CLI `AfterTool` hooks.
/// - Claude: includes `hookEventName` + `additionalContext` for Claude Code
///   hooks (required by the Claude Code hook contract).
fn format_diagnostics(lines: &[String], format: HostFormat, hook_event: &str) -> String {
    let diagnostics = lines.join("\n");
    // serde_json::to_string cannot fail on Value
    match format {
        HostFormat::Gemini => serde_json::json!({
            "hookSpecificOutput": {
                "additionalContext": format!("LSP Diagnostics:\n{diagnostics}")
            }
        })
        .to_string(),
        HostFormat::Claude => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": hook_event,
                "additionalContext": diagnostics
            }
        })
        .to_string(),
    }
}

/// Expected Claude Code hooks, embedded at compile time.
const CLAUDE_HOOKS_EXPECTED: &str = include_str!("../plugins/catenary/hooks/hooks.json");

/// Expected Gemini CLI hooks, embedded at compile time.
const GEMINI_HOOKS_EXPECTED: &str = include_str!("../hooks/hooks.json");

/// Check Claude Code plugin hooks against the embedded expected hooks.
fn check_claude_hooks(colors: &ColorConfig) {
    let label = format!("{:<14}", "Claude Code");
    let Ok(home_str) = std::env::var("HOME") else {
        println!(
            "  {label}{}",
            colors.dim("- cannot determine home directory"),
        );
        return;
    };
    let home = PathBuf::from(home_str);

    let plugins_file = home.join(".claude/plugins/installed_plugins.json");
    let Ok(plugins_json) = std::fs::read_to_string(&plugins_file) else {
        println!("  {label}{}", colors.dim("- not installed"));
        return;
    };

    let Ok(plugins) = serde_json::from_str::<serde_json::Value>(&plugins_json) else {
        println!(
            "  {label}{}",
            colors.yellow("? cannot parse installed_plugins.json"),
        );
        return;
    };

    // Look up catenary@catenary in plugins.plugins
    let entries = match plugins
        .get("plugins")
        .and_then(|p| p.get("catenary@catenary"))
        .and_then(serde_json::Value::as_array)
    {
        Some(arr) if !arr.is_empty() => arr,
        _ => {
            println!("  {label}{}", colors.dim("- not installed"));
            return;
        }
    };

    // Use the first (most recent) entry
    let entry = &entries[0];
    let version = entry
        .get("version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let Some(install_path_str) = entry.get("installPath").and_then(serde_json::Value::as_str)
    else {
        println!(
            "  {label}{version:<8}{}",
            colors.yellow("? missing installPath"),
        );
        return;
    };
    let install_path = PathBuf::from(install_path_str);

    // Determine marketplace source type
    let source_type = read_marketplace_source(&home);
    let version_display = source_type
        .as_deref()
        .map_or_else(|| version.to_string(), |src| format!("{version} ({src})"));
    let ver_col = format!("{version_display:<20}");

    // Read installed hooks and compare
    let hooks_path = install_path.join("hooks/hooks.json");
    match std::fs::read_to_string(&hooks_path) {
        Ok(installed) => {
            if normalize_json(&installed) == normalize_json(CLAUDE_HOOKS_EXPECTED) {
                println!("  {label}{ver_col}{}", colors.green("✓ hooks match"));
            } else {
                println!(
                    "  {label}{ver_col}{}",
                    colors.red("✗ stale hooks (reinstall: claude plugin uninstall catenary@catenary && claude plugin install catenary@catenary)"),
                );
            }
        }
        Err(_) => {
            println!(
                "  {label}{ver_col}{}",
                colors.red("✗ hooks.json not found in plugin cache"),
            );
        }
    }
}

/// Read the catenary marketplace source type from `known_marketplaces.json`.
fn read_marketplace_source(home: &Path) -> Option<String> {
    let path = home.join(".claude/plugins/known_marketplaces.json");
    let contents = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&contents).ok()?;
    json.get("catenary")
        .and_then(|c| c.get("source"))
        .and_then(|s| s.get("source"))
        .and_then(serde_json::Value::as_str)
        .map(std::string::ToString::to_string)
}

/// Check Gemini CLI extension hooks against the embedded expected hooks.
fn check_gemini_hooks(colors: &ColorConfig) {
    let label = format!("{:<14}", "Gemini CLI");
    let Ok(home_str) = std::env::var("HOME") else {
        println!(
            "  {label}{}",
            colors.dim("- cannot determine home directory"),
        );
        return;
    };
    let home = PathBuf::from(home_str);

    // Look for the extension directory
    let ext_dir = home.join(".gemini/extensions");
    let candidates = ["Catenary", "catenary"];
    let ext_path = candidates
        .iter()
        .map(|name| ext_dir.join(name))
        .find(|p| p.is_dir());

    let Some(ext_path) = ext_path else {
        println!("  {label}{}", colors.dim("- not installed"));
        return;
    };

    // Read .gemini-extension-install.json to determine install type and source.
    // Gemini CLI writes this metadata file for both linked and installed extensions.
    let install_meta_path = ext_path.join(".gemini-extension-install.json");
    let install_meta = std::fs::read_to_string(&install_meta_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

    let install_type = install_meta
        .as_ref()
        .and_then(|m| m.get("type").and_then(serde_json::Value::as_str))
        .unwrap_or("unknown");

    // For linked extensions, the source field is a local path to the actual
    // extension files. For installed extensions (github-release, etc.), the
    // files are cloned into the extension directory itself.
    let resolved = if install_type == "link" {
        install_meta
            .as_ref()
            .and_then(|m| m.get("source").and_then(serde_json::Value::as_str))
            .map_or_else(|| ext_path.clone(), PathBuf::from)
    } else {
        ext_path
    };

    // Read the extension manifest for version info
    let manifest_path = resolved.join("gemini-extension.json");
    let version = std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("version")
                .and_then(serde_json::Value::as_str)
                .map(std::string::ToString::to_string)
        });

    let type_label = if install_type == "link" {
        "linked"
    } else {
        "installed"
    };
    let version_display = version
        .as_deref()
        .map_or_else(|| type_label.to_string(), |v| format!("{v} ({type_label})"));
    let ver_col = format!("{version_display:<20}");

    // Read hooks and compare against embedded
    let hooks_path = resolved.join("hooks/hooks.json");
    match std::fs::read_to_string(&hooks_path) {
        Ok(installed) => {
            if normalize_json(&installed) == normalize_json(GEMINI_HOOKS_EXPECTED) {
                println!("  {label}{ver_col}{}", colors.green("✓ hooks match"));
            } else {
                println!(
                    "  {label}{ver_col}{}",
                    colors.red("✗ stale hooks (update extension)"),
                );
            }
        }
        Err(_) => {
            println!(
                "  {label}{ver_col}{}",
                colors.yellow("? hooks.json not found"),
            );
        }
    }
}

/// Check whether the running binary matches what `$PATH` would resolve.
fn check_path_binary(colors: &ColorConfig) {
    let label = format!("{:<14}", "PATH");
    let spacer = " ".repeat(20);

    let Some(current_exe) = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
    else {
        println!(
            "  {label}{}",
            colors.yellow("? cannot determine current executable"),
        );
        return;
    };

    // Find catenary on PATH
    let path_var = std::env::var("PATH").unwrap_or_default();
    let Some(path_binary) = std::env::split_paths(&path_var)
        .map(|dir| dir.join("catenary"))
        .find(|p| p.is_file())
    else {
        println!(
            "  {label}{spacer}{}",
            colors.red("✗ catenary not found on PATH"),
        );
        return;
    };

    let resolved_path = std::fs::canonicalize(&path_binary).unwrap_or(path_binary);

    if current_exe == resolved_path {
        println!(
            "  {label}{spacer}{}",
            colors.green(&format!("✓ {}", resolved_path.display())),
        );
    } else {
        println!(
            "  {label}{spacer}{}",
            colors.red(&format!(
                "✗ {} differs from {}",
                resolved_path.display(),
                current_exe.display(),
            )),
        );
    }
}

/// Normalize a JSON string for comparison (parse and re-serialize).
///
/// Returns the compact re-serialized form, or the original string (trimmed)
/// if parsing fails.
fn normalize_json(s: &str) -> String {
    serde_json::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| serde_json::to_string(&v).ok())
        .unwrap_or_else(|| s.trim().to_string())
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

    // Print version header
    println!("Catenary {}", env!("CATENARY_VERSION"));
    println!();

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

    // Hooks health section
    println!();
    println!("{}:", colors.bold("Hooks"));
    check_claude_hooks(&colors);
    check_gemini_hooks(&colors);
    check_path_binary(&colors);

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
    let time = event.timestamp.with_timezone(&Local).format("%H:%M:%S");

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
#[allow(clippy::too_many_lines, reason = "Match arms for each event kind")]
fn print_event_annotated(event: &SessionEvent, colors: &ColorConfig, term_width: usize) {
    let time = event.timestamp.with_timezone(&Local).format("%H:%M:%S");
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
        EventKind::Diagnostics {
            file,
            count,
            preview,
        } => {
            let basename = std::path::Path::new(file)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(file);
            if *count == 0 {
                let check = colors.green("ok");
                println!("{time_str} {basename}: {check}");
            } else {
                let label = colors.yellow(&format!(
                    "{count} diagnostic{}",
                    if *count == 1 { "" } else { "s" }
                ));
                let detail = if preview.is_empty() {
                    String::new()
                } else {
                    let max_len = term_width.saturating_sub(14 + basename.len() + 20);
                    format!(" -- {}", cli::truncate(preview, max_len))
                };
                println!("{time_str} {basename}: {label}{detail}");
            }
        }
        EventKind::LockAcquired { file, owner } => {
            let basename = std::path::Path::new(file.as_str())
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(file);
            let lock_icon = colors.green("locked");
            let short_owner = cli::truncate(owner, 20);
            println!("{time_str} {basename}: {lock_icon} by {short_owner}");
        }
        EventKind::LockReleased { file, owner } => {
            let basename = std::path::Path::new(file.as_str())
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(file);
            let unlock_icon = colors.dim("unlocked");
            let short_owner = cli::truncate(owner, 20);
            println!("{time_str} {basename}: {unlock_icon} by {short_owner}");
        }
        EventKind::LockDenied {
            file,
            owner,
            held_by,
        } => {
            let basename = std::path::Path::new(file.as_str())
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(file);
            let denied = colors.red("lock denied");
            let short_owner = cli::truncate(owner, 20);
            let short_held = cli::truncate(held_by, 20);
            println!("{time_str} {basename}: {denied} for {short_owner} (held by {short_held})");
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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn test_format_diagnostics_claude() -> Result<()> {
        let lines = vec![
            "error[E0308]: mismatched types".into(),
            "  --> src/main.rs:5:10".into(),
        ];
        let output = format_diagnostics(&lines, HostFormat::Claude, "PostToolUse");
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("claude format should produce valid JSON")?;

        let hook_output = &parsed["hookSpecificOutput"];
        assert_eq!(hook_output["hookEventName"], "PostToolUse");
        let context = hook_output["additionalContext"]
            .as_str()
            .context("additionalContext should be a string")?;
        assert!(context.contains("error[E0308]: mismatched types"));
        assert!(context.contains("  --> src/main.rs:5:10"));
        // Claude format should NOT have the "LSP Diagnostics:" prefix
        assert!(!context.starts_with("LSP Diagnostics:"));
        Ok(())
    }

    #[test]
    fn test_format_diagnostics_gemini() -> Result<()> {
        let lines = vec!["error[E0308]: mismatched types".into()];
        let output = format_diagnostics(&lines, HostFormat::Gemini, "PostToolUse");
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("gemini format should produce valid JSON")?;

        let context = parsed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .context("additionalContext should be a string")?;
        assert!(context.starts_with("LSP Diagnostics:\n"));
        assert!(context.contains("error[E0308]: mismatched types"));
        // Gemini format should NOT have hookEventName
        assert!(parsed["hookSpecificOutput"]["hookEventName"].is_null());
        Ok(())
    }

    #[test]
    fn test_format_diagnostics_gemini_multiline() -> Result<()> {
        let lines = vec!["warning: unused variable".into(), "  --> lib.rs:3:9".into()];
        let output = format_diagnostics(&lines, HostFormat::Gemini, "PostToolUse");
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;
        let context = parsed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .context("additionalContext should be a string")?;
        assert!(context.contains("warning: unused variable\n  --> lib.rs:3:9"));
        Ok(())
    }

    #[test]
    fn test_format_diagnostics_claude_propagates_hook_event() -> Result<()> {
        let lines = vec!["Added roots: /tmp/foo".into()];
        let output = format_diagnostics(&lines, HostFormat::Claude, "PreToolUse");
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;

        assert_eq!(parsed["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        Ok(())
    }

    #[test]
    fn test_format_lock_output_claude_stale_read() -> Result<()> {
        let output = format_lock_output(
            HostFormat::Claude,
            Some("File was modified since your last read"),
            None,
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;

        let hook_output = &parsed["hookSpecificOutput"];
        assert_eq!(hook_output["hookEventName"], "PreToolUse");
        assert_eq!(hook_output["permissionDecision"], "allow");
        assert_eq!(
            hook_output["additionalContext"],
            "File was modified since your last read"
        );
        Ok(())
    }

    #[test]
    fn test_format_lock_output_claude_denied() -> Result<()> {
        let output =
            format_lock_output(HostFormat::Claude, None, Some("Lock held by another agent"));
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;

        let hook_output = &parsed["hookSpecificOutput"];
        assert_eq!(hook_output["hookEventName"], "PreToolUse");
        assert_eq!(hook_output["permissionDecision"], "deny");
        assert_eq!(
            hook_output["permissionDecisionReason"],
            "Lock held by another agent"
        );
        Ok(())
    }

    #[test]
    fn test_format_lock_output_gemini_stale_read() -> Result<()> {
        let output = format_lock_output(
            HostFormat::Gemini,
            Some("File was modified since your last read"),
            None,
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;

        assert_eq!(parsed["decision"], "deny");
        assert_eq!(parsed["reason"], "File was modified since your last read");
        // Gemini should not have hookSpecificOutput
        assert!(parsed["hookSpecificOutput"].is_null());
        Ok(())
    }

    #[test]
    fn test_format_lock_output_gemini_denied() -> Result<()> {
        let output =
            format_lock_output(HostFormat::Gemini, None, Some("Lock held by another agent"));
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;

        assert_eq!(parsed["decision"], "deny");
        assert_eq!(parsed["reason"], "Lock held by another agent");
        assert!(parsed["hookSpecificOutput"].is_null());
        Ok(())
    }

    #[test]
    fn test_format_lock_output_no_output() {
        let output = format_lock_output(HostFormat::Claude, None, None);
        assert!(output.is_empty());

        let output = format_lock_output(HostFormat::Gemini, None, None);
        assert!(output.is_empty());
    }
}

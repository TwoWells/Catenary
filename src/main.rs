/*
 * Copyright (C) 2026 Mark Wells Dev
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

use anyhow::Result;
use chrono::Utc;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use catenary_mcp::bridge::{DocumentManager, LspBridgeHandler};
use catenary_mcp::lsp;
use catenary_mcp::mcp::McpServer;
use catenary_mcp::session::{self, EventKind, Session};

#[derive(Parser, Debug)]
#[command(name = "catenary")]
#[command(about = "Multiplexing bridge between MCP and multiple LSP servers")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// LSP servers to spawn in "lang:command" format (e.g., "rust:rust-analyzer")
    /// Can be specified multiple times. These override/append to the config file.
    #[arg(short, long = "lsp", global = true)]
    lsps: Vec<String>,

    /// Path to configuration file
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Workspace root directory
    #[arg(short, long, default_value = ".", global = true)]
    root: PathBuf,

    /// Document idle timeout in seconds before auto-close (0 to disable)
    /// Overrides config file if set (default in config is 300)
    #[arg(long, global = true)]
    idle_timeout: Option<u64>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the MCP server (default if no subcommand given)
    Serve,

    /// List active Catenary sessions
    List,

    /// Monitor events from a session
    Monitor {
        /// Session ID (use 'catenary list' to see available sessions)
        id: String,
    },

    /// Show status of a session
    Status {
        /// Session ID (use 'catenary list' to see available sessions)
        id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    match args.command {
        None | Some(Command::Serve) => run_server(args).await,
        Some(Command::List) => run_list(),
        Some(Command::Monitor { id }) => run_monitor(&id),
        Some(Command::Status { id }) => run_status(&id),
    }
}

/// Run the MCP server (main functionality)
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
            anyhow::anyhow!("Invalid LSP spec: {}. Expected 'lang:command'", lsp_spec)
        })?;

        let lang = lang.trim().to_string();
        let command_str = command_str.trim();

        // Parse command into program and arguments
        let mut parts = command_str.split_whitespace();
        let program = parts.next().expect("command cannot be empty").to_string();
        let cmd_args: Vec<String> = parts.map(|s| s.to_string()).collect();

        config.server.insert(
            lang,
            catenary_mcp::config::ServerConfig {
                command: program,
                args: cmd_args,
                initialization_options: None,
            },
        );
    }

    let root = args.root.canonicalize()?;

    // Create session for observability
    let session = Session::create(root.to_string_lossy().as_ref())?;
    let broadcaster = session.broadcaster();

    info!("Starting catenary multiplexing bridge");
    info!("Session ID: {}", session.info.id);
    info!("Workspace root: {}", root.display());
    info!("Document idle timeout: {}s", config.idle_timeout);

    // Create managers
    let client_manager = Arc::new(lsp::ClientManager::new(
        config.clone(),
        root,
        broadcaster.clone(),
    ));
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

    let handler = LspBridgeHandler::new(
        client_manager.clone(),
        doc_manager,
        runtime,
        config,
        broadcaster.clone(),
    );

    // Run MCP server (blocking - reads from stdin)
    let mut mcp_server = McpServer::new(handler, broadcaster);

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
fn run_list() -> Result<()> {
    let sessions = session::list_sessions()?;

    if sessions.is_empty() {
        println!("No active Catenary sessions");
        return Ok(());
    }

    // Print header
    println!(
        "{:<12} {:<8} {:<40} {:<20} STARTED",
        "ID", "PID", "WORKSPACE", "CLIENT"
    );
    println!("{}", "-".repeat(100));

    for s in sessions {
        let client = match (&s.client_name, &s.client_version) {
            (Some(name), Some(ver)) => format!("{} v{}", name, ver),
            (Some(name), None) => name.clone(),
            _ => "(unknown)".to_string(),
        };

        let ago = format_duration_ago(s.started_at);

        // Truncate workspace if too long
        let workspace = if s.workspace.len() > 38 {
            format!("...{}", &s.workspace[s.workspace.len() - 35..])
        } else {
            s.workspace.clone()
        };

        println!(
            "{:<12} {:<8} {:<40} {:<20} {}",
            s.id, s.pid, workspace, client, ago
        );
    }

    Ok(())
}

/// Monitor events from a session
fn run_monitor(id: &str) -> Result<()> {
    // Find session (support prefix matching)
    let session = find_session(id)?;
    let full_id = session.id;

    println!("Monitoring session {} (Ctrl+C to stop)\n", full_id);

    let mut reader = session::tail_events(&full_id)?;

    loop {
        match reader.next_event()? {
            Some(event) => {
                print_event(&event);
            }
            None => {
                println!("\nSession ended");
                break;
            }
        }
    }

    Ok(())
}

/// Show status of a session
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
        print!("Client: {}", name);
        if let Some(ver) = &session.client_version {
            print!(" v{}", ver);
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
        0 => anyhow::bail!("No session found matching '{}'", id),
        1 => Ok(matches[0].clone()),
        _ => {
            eprintln!("Multiple sessions match '{}':", id);
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

/// Print an event in human-readable format
fn print_event(event: &session::SessionEvent) {
    let time = event.timestamp.format("%H:%M:%S");

    match &event.kind {
        EventKind::Started => {
            println!("[{}] Session started", time);
        }
        EventKind::Shutdown => {
            println!("[{}] Session shutting down", time);
        }
        EventKind::ServerState { language, state } => {
            println!("[{}] {}: {}", time, language, state);
        }
        EventKind::Progress {
            language,
            title,
            message,
            percentage,
        } => {
            let pct = percentage.map(|p| format!(" {}%", p)).unwrap_or_default();
            let msg = message
                .as_ref()
                .map(|m| format!(" ({})", m))
                .unwrap_or_default();
            println!("[{}] {}: {}{}{}", time, language, title, pct, msg);
        }
        EventKind::ProgressEnd { language } => {
            println!("[{}] {}: Ready", time, language);
        }
        EventKind::ToolCall { tool, file } => {
            let file_str = file
                .as_ref()
                .map(|f| format!(" on {}", f))
                .unwrap_or_default();
            println!("[{}] Tool: {}{}", time, tool, file_str);
        }
        EventKind::ToolResult {
            tool,
            success,
            duration_ms,
        } => {
            let status = if *success { "ok" } else { "error" };
            println!(
                "[{}] Tool: {} -> {} ({}ms)",
                time, tool, status, duration_ms
            );
        }
        EventKind::McpMessage { direction, message } => {
            let json_str = serde_json::to_string(message).unwrap_or_default();
            println!("[{}] MCP({}): {}", time, direction, json_str);
        }
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

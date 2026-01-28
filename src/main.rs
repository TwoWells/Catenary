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
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use catenary_mcp::bridge::{DocumentManager, LspBridgeHandler};
use catenary_mcp::lsp;
use catenary_mcp::mcp::McpServer;

#[derive(Parser, Debug)]
#[command(name = "catenary")]
#[command(about = "Multiplexing bridge between MCP and multiple LSP servers")]
struct Args {
    /// LSP servers to spawn in "lang:command" format (e.g., "rust:rust-analyzer")
    /// Can be specified multiple times. These override/append to the config file.
    #[arg(short, long = "lsp")]
    lsps: Vec<String>,

    /// Path to configuration file
    #[arg(long)]
    config: Option<PathBuf>,

    /// Workspace root directory
    #[arg(short, long, default_value = ".")]
    root: PathBuf,

    /// Document idle timeout in seconds before auto-close (0 to disable)
    /// Overrides config file if set (default in config is 300)
    #[arg(long)]
    idle_timeout: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("catenary=info".parse()?))
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

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
    info!("Starting catenary multiplexing bridge");
    info!("Workspace root: {}", root.display());
    info!("Document idle timeout: {}s", config.idle_timeout);

    // Create managers
    let client_manager = Arc::new(lsp::ClientManager::new(config.clone(), root));
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

    let handler = LspBridgeHandler::new(client_manager.clone(), doc_manager, runtime);

    // Run MCP server (blocking - reads from stdin)
    let mut mcp_server = McpServer::new(handler);

    // Run in a blocking task since MCP server uses synchronous I/O
    let mcp_result = tokio::task::spawn_blocking(move || mcp_server.run()).await?;

    // Stop cleanup task
    if let Some(handle) = cleanup_handle {
        handle.abort();
        let _ = handle.await;
    }

    // Shutdown LSP clients
    info!("Shutting down LSP servers");
    client_manager.shutdown_all().await;

    mcp_result
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

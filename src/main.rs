use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

mod bridge;
mod lsp;
mod mcp;

use bridge::{DocumentManager, LspBridgeHandler};
use mcp::McpServer;

#[derive(Parser, Debug)]
#[command(name = "catenary")]
#[command(about = "Bridge between MCP and LSP servers")]
struct Args {
    /// The LSP server command to spawn (e.g., "gopls" or "rust-analyzer")
    #[arg(short, long)]
    command: String,

    /// Workspace root directory
    #[arg(short, long, default_value = ".")]
    root: PathBuf,

    /// Document idle timeout in seconds before auto-close (0 to disable)
    #[arg(long, default_value = "300")]
    idle_timeout: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("catenary=info".parse()?))
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    let root = args.root.canonicalize()?;
    info!("Starting catenary bridge");
    info!("LSP command: {}", args.command);
    info!("Workspace root: {}", root.display());
    info!("Document idle timeout: {}s", args.idle_timeout);

    // Parse command into program and arguments
    let mut parts = args.command.split_whitespace();
    let program = parts.next().expect("command cannot be empty");
    let cmd_args: Vec<&str> = parts.collect();

    // Spawn LSP client
    let mut client = lsp::LspClient::spawn(program, &cmd_args).await?;
    let capabilities = client.initialize(&root).await?;

    info!("LSP server initialized");
    info!(
        "Hover support: {}",
        capabilities.capabilities.hover_provider.is_some()
    );
    info!(
        "Definition support: {}",
        capabilities.capabilities.definition_provider.is_some()
    );

    // Create bridge components
    let client = Arc::new(Mutex::new(client));
    let doc_manager = Arc::new(Mutex::new(DocumentManager::new()));
    let runtime = tokio::runtime::Handle::current();

    // Start document cleanup task if timeout is enabled
    let cleanup_handle = if args.idle_timeout > 0 {
        let client_clone = client.clone();
        let doc_manager_clone = doc_manager.clone();
        let idle_timeout = args.idle_timeout;

        Some(tokio::spawn(async move {
            document_cleanup_task(client_clone, doc_manager_clone, idle_timeout).await;
        }))
    } else {
        None
    };

    let handler = LspBridgeHandler::new(client.clone(), doc_manager, runtime);

    // Run MCP server (blocking - reads from stdin)
    let mut mcp_server = McpServer::new(handler);

    // Run in a blocking task since MCP server uses synchronous I/O
    let mcp_result = tokio::task::spawn_blocking(move || mcp_server.run()).await?;

    // Stop cleanup task
    if let Some(handle) = cleanup_handle {
        handle.abort();
    }

    // Shutdown LSP client
    info!("Shutting down LSP server");
    let mut client = Arc::try_unwrap(client)
        .map_err(|_| anyhow::anyhow!("Failed to unwrap client Arc"))?
        .into_inner();
    client.shutdown().await?;

    mcp_result
}

/// Background task that periodically closes idle documents.
async fn document_cleanup_task(
    client: Arc<Mutex<lsp::LspClient>>,
    doc_manager: Arc<Mutex<DocumentManager>>,
    idle_timeout_secs: u64,
) {
    // Check every 60 seconds or half the timeout, whichever is smaller
    let check_interval = Duration::from_secs(idle_timeout_secs.min(60));

    loop {
        tokio::time::sleep(check_interval).await;

        // Check if LSP server is still alive
        {
            let client = client.lock().await;
            if !client.is_alive() {
                warn!("LSP server is no longer alive, stopping cleanup task");
                break;
            }
        }

        // Find and close stale documents
        let stale_paths = {
            let doc_manager = doc_manager.lock().await;
            doc_manager.stale_documents(idle_timeout_secs)
        };

        if !stale_paths.is_empty() {
            debug!("Closing {} stale documents", stale_paths.len());

            for path in stale_paths {
                let close_params = {
                    let mut doc_manager = doc_manager.lock().await;
                    doc_manager.close(&path)
                };

                if let Ok(Some(params)) = close_params {
                    let client = client.lock().await;
                    if let Err(e) = client.did_close(params).await {
                        warn!("Failed to close document {}: {}", path.display(), e);
                    } else {
                        debug!("Closed stale document: {}", path.display());
                    }
                }
            }
        }
    }
}

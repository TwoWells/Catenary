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

use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::Config;
use crate::lsp::LspClient;

/// Manages the lifecycle of LSP clients (lazy spawning, caching, shutdown).
pub struct ClientManager {
    config: Config,
    root: PathBuf,
    active_clients: Mutex<HashMap<String, Arc<Mutex<LspClient>>>>,
}

impl ClientManager {
    pub fn new(config: Config, root: PathBuf) -> Self {
        Self {
            config,
            root,
            active_clients: Mutex::new(HashMap::new()),
        }
    }

    /// Gets an active client for the given language, spawning it if necessary.
    pub async fn get_client(&self, lang: &str) -> Result<Arc<Mutex<LspClient>>> {
        let mut clients = self.active_clients.lock().await;

        if let Some(client) = clients.get(lang) {
            // Check if it's still alive
            let is_alive = {
                let c = client.lock().await;
                c.is_alive()
            };

            if is_alive {
                return Ok(client.clone());
            } else {
                warn!("LSP server for {} died, restarting...", lang);
                clients.remove(lang);
            }
        }

        // Spawn new client
        let server_config = self
            .config
            .server
            .get(lang)
            .ok_or_else(|| anyhow!("No LSP server configured for language '{}'", lang))?;

        info!(
            "Spawning LSP server for {}: {} {}",
            lang,
            server_config.command,
            server_config.args.join(" ")
        );

        let args: Vec<&str> = server_config
            .args
            .iter()
            .map(|s: &String| s.as_str())
            .collect();
        let mut client = LspClient::spawn(&server_config.command, &args).await?;

        // Initialize
        // TODO: Pass initialization options from config when supported
        client.initialize(&self.root).await?;

        let client_mutex = Arc::new(Mutex::new(client));
        clients.insert(lang.to_string(), client_mutex.clone());

        Ok(client_mutex)
    }

    /// Returns a snapshot of all currently active clients.
    pub async fn active_clients(&self) -> HashMap<String, Arc<Mutex<LspClient>>> {
        self.active_clients.lock().await.clone()
    }

    /// Shuts down a specific client if it exists.
    pub async fn shutdown_client(&self, lang: &str) {
        let mut clients = self.active_clients.lock().await;
        if let Some(client_mutex) = clients.remove(lang) {
            info!("Shutting down idle LSP server for {}", lang);
            let mut client = client_mutex.lock().await;
            if client.is_alive()
                && let Err(e) = client.shutdown().await
            {
                warn!("Failed to shutdown LSP server for {}: {}", lang, e);
            }
        }
    }

    /// Shuts down all active clients.
    pub async fn shutdown_all(&self) {
        let mut clients = self.active_clients.lock().await;
        for (lang, client_mutex) in clients.drain() {
            // We need to unwrap the Arc if possible, or lock and shutdown.
            // Since we are shutting down the manager, we likely own the last references
            // if other tasks (like handlers) have finished.
            // However, handlers might still hold references.

            // Just lock and shutdown. LspClient::shutdown handles repeated calls gracefully?
            // LspClient::shutdown sends "shutdown" request.

            // Ideally we try_unwrap, but locking is safer if there are stragglers.
            {
                let mut client = client_mutex.lock().await;
                if client.is_alive()
                    && let Err(e) = client.shutdown().await
                {
                    warn!("Failed to shutdown LSP server for {}: {}", lang, e);
                }
            }
        }
    }
}

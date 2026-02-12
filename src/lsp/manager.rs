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
use crate::lsp::state::ServerStatus;
use crate::session::EventBroadcaster;

/// Manages the lifecycle of LSP clients (lazy spawning, caching, shutdown).
pub struct ClientManager {
    config: Config,
    root: PathBuf,
    active_clients: Mutex<HashMap<String, Arc<Mutex<LspClient>>>>,
    broadcaster: EventBroadcaster,
}

impl ClientManager {
    /// Creates a new `ClientManager`.
    #[must_use]
    pub fn new(config: Config, root: PathBuf, broadcaster: EventBroadcaster) -> Self {
        Self {
            config,
            root,
            active_clients: Mutex::new(HashMap::new()),
            broadcaster,
        }
    }

    /// Gets an active client for the given language, spawning it if necessary.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No LSP server is configured for the language.
    /// - The server fails to spawn.
    /// - The server fails to initialize.
    pub async fn get_client(&self, lang: &str) -> Result<Arc<Mutex<LspClient>>> {
        if let Some(client) = self.active_clients.lock().await.get(lang) {
            // Check if it's still alive
            let is_alive = client.lock().await.is_alive();

            if is_alive {
                return Ok(client.clone());
            }
            warn!("LSP server for {} died, restarting...", lang);
            self.active_clients.lock().await.remove(lang);
        }

        let mut clients = self.active_clients.lock().await;

        // Spawn new client
        let server_config = self
            .config
            .server
            .get(lang)
            .ok_or_else(|| anyhow!("No LSP server configured for language '{lang}'"))?;

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
        let mut client = LspClient::spawn(
            &server_config.command,
            &args,
            lang,
            self.broadcaster.clone(),
        )?;

        // Initialize
        // TODO: Pass initialization options from config when supported
        client.initialize(&self.root).await?;

        let client_mutex = Arc::new(Mutex::new(client));
        clients.insert(lang.to_string(), client_mutex.clone());
        drop(clients);

        Ok(client_mutex)
    }

    /// Returns a snapshot of all currently active clients.
    pub async fn active_clients(&self) -> HashMap<String, Arc<Mutex<LspClient>>> {
        self.active_clients.lock().await.clone()
    }

    /// Returns status of all active servers.
    pub async fn all_server_status(&self) -> Vec<ServerStatus> {
        let clients = self.active_clients.lock().await.clone();
        let mut statuses = Vec::new();

        for (lang, client_mutex) in clients {
            let status = client_mutex.lock().await.status(lang).await;
            statuses.push(status);
        }

        statuses
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

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
use std::path::{Path, PathBuf};
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
    roots: Mutex<Vec<PathBuf>>,
    active_clients: Mutex<HashMap<String, Arc<Mutex<LspClient>>>>,
    broadcaster: EventBroadcaster,
}

impl ClientManager {
    /// Creates a new `ClientManager`.
    #[must_use]
    pub fn new(config: Config, roots: Vec<PathBuf>, broadcaster: EventBroadcaster) -> Self {
        Self {
            config,
            roots: Mutex::new(roots),
            active_clients: Mutex::new(HashMap::new()),
            broadcaster,
        }
    }

    /// Returns the current workspace roots.
    pub async fn roots(&self) -> Vec<PathBuf> {
        self.roots.lock().await.clone()
    }

    /// Adds a new workspace root and notifies all active LSP clients.
    ///
    /// # Errors
    ///
    /// Returns an error if the root path cannot be converted to a valid URI.
    pub async fn add_root(&self, root: PathBuf) -> Result<()> {
        let uri: lsp_types::Uri = format!("file://{}", root.display())
            .parse()
            .map_err(|e| anyhow!("Invalid root path {}: {e}", root.display()))?;

        let folder = lsp_types::WorkspaceFolder {
            uri,
            name: root.file_name().map_or_else(
                || "workspace".to_string(),
                |s| s.to_string_lossy().to_string(),
            ),
        };

        self.roots.lock().await.push(root);

        // Notify all active clients
        let clients = self.active_clients.lock().await.clone();
        for (lang, client_mutex) in clients {
            let client = client_mutex.lock().await;
            if client.is_alive()
                && let Err(e) = client
                    .did_change_workspace_folders(vec![folder.clone()], vec![])
                    .await
            {
                warn!(
                    "Failed to notify {} server about new workspace folder: {}",
                    lang, e
                );
            }
        }

        Ok(())
    }

    /// Removes a workspace root and notifies all active LSP clients.
    ///
    /// # Errors
    ///
    /// Returns an error if the root path cannot be converted to a valid URI.
    pub async fn remove_root(&self, root: &Path) -> Result<()> {
        let uri: lsp_types::Uri = format!("file://{}", root.display())
            .parse()
            .map_err(|e| anyhow!("Invalid root path {}: {e}", root.display()))?;

        let folder = lsp_types::WorkspaceFolder {
            uri,
            name: root.file_name().map_or_else(
                || "workspace".to_string(),
                |s| s.to_string_lossy().to_string(),
            ),
        };

        self.roots.lock().await.retain(|r| r != root);

        // Notify all active clients
        let clients = self.active_clients.lock().await.clone();
        for (lang, client_mutex) in clients {
            let client = client_mutex.lock().await;
            if client.is_alive()
                && let Err(e) = client
                    .did_change_workspace_folders(vec![], vec![folder.clone()])
                    .await
            {
                warn!(
                    "Failed to notify {} server about removed workspace folder: {}",
                    lang, e
                );
            }
        }

        Ok(())
    }

    /// Synchronizes workspace roots with a new set.
    ///
    /// Diffs against current roots: adds new ones, removes stale ones.
    /// Sends a single `didChangeWorkspaceFolders` notification per client
    /// with both additions and removals.
    ///
    /// # Errors
    ///
    /// Returns an error if any root path cannot be converted to a valid URI.
    pub async fn sync_roots(&self, new_roots: Vec<PathBuf>) -> Result<()> {
        let current_roots = self.roots.lock().await.clone();

        let to_add: Vec<&PathBuf> = new_roots
            .iter()
            .filter(|r| !current_roots.contains(r))
            .collect();
        let to_remove: Vec<&PathBuf> = current_roots
            .iter()
            .filter(|r| !new_roots.contains(r))
            .collect();

        if to_add.is_empty() && to_remove.is_empty() {
            return Ok(());
        }

        info!(
            "Syncing roots: {} added, {} removed",
            to_add.len(),
            to_remove.len()
        );

        let added_folders = to_add
            .iter()
            .map(|root| {
                let uri: lsp_types::Uri = format!("file://{}", root.display())
                    .parse()
                    .map_err(|e| anyhow!("Invalid root path {}: {e}", root.display()))?;
                Ok(lsp_types::WorkspaceFolder {
                    uri,
                    name: root.file_name().map_or_else(
                        || "workspace".to_string(),
                        |s| s.to_string_lossy().to_string(),
                    ),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let removed_folders = to_remove
            .iter()
            .map(|root| {
                let uri: lsp_types::Uri = format!("file://{}", root.display())
                    .parse()
                    .map_err(|e| anyhow!("Invalid root path {}: {e}", root.display()))?;
                Ok(lsp_types::WorkspaceFolder {
                    uri,
                    name: root.file_name().map_or_else(
                        || "workspace".to_string(),
                        |s| s.to_string_lossy().to_string(),
                    ),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        // Update internal state
        *self.roots.lock().await = new_roots;

        // Notify all active clients
        let clients = self.active_clients.lock().await.clone();
        for (lang, client_mutex) in clients {
            let client = client_mutex.lock().await;
            if client.is_alive()
                && let Err(e) = client
                    .did_change_workspace_folders(added_folders.clone(), removed_folders.clone())
                    .await
            {
                warn!(
                    "Failed to notify {} server about workspace folder changes: {}",
                    lang, e
                );
            }
        }

        Ok(())
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
        let roots = self.roots.lock().await.clone();
        client.initialize(&roots).await?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    fn test_config() -> Config {
        Config {
            server: HashMap::new(),
            idle_timeout: 300,
            smart_wait: true,
        }
    }

    #[tokio::test]
    async fn test_roots_returns_initial_roots() -> Result<()> {
        let broadcaster = EventBroadcaster::noop()?;
        let manager = ClientManager::new(
            test_config(),
            vec![PathBuf::from("/tmp/root_a"), PathBuf::from("/tmp/root_b")],
            broadcaster,
        );

        let roots = manager.roots().await;
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/tmp/root_a"));
        assert_eq!(roots[1], PathBuf::from("/tmp/root_b"));
        Ok(())
    }

    #[tokio::test]
    async fn test_add_root_appends() -> Result<()> {
        let broadcaster = EventBroadcaster::noop()?;
        let manager = ClientManager::new(
            test_config(),
            vec![PathBuf::from("/tmp/root_a")],
            broadcaster,
        );

        assert_eq!(manager.roots().await.len(), 1);

        // add_root with no active clients should succeed silently
        manager.add_root(PathBuf::from("/tmp/root_b")).await?;

        let roots = manager.roots().await;
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[1], PathBuf::from("/tmp/root_b"));
        Ok(())
    }

    #[tokio::test]
    async fn test_roots_empty_initial() -> Result<()> {
        let broadcaster = EventBroadcaster::noop()?;
        let manager = ClientManager::new(test_config(), vec![], broadcaster);

        assert!(manager.roots().await.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_remove_root() -> Result<()> {
        let broadcaster = EventBroadcaster::noop()?;
        let manager = ClientManager::new(
            test_config(),
            vec![PathBuf::from("/tmp/root_a"), PathBuf::from("/tmp/root_b")],
            broadcaster,
        );

        assert_eq!(manager.roots().await.len(), 2);

        manager.remove_root(Path::new("/tmp/root_a")).await?;

        let roots = manager.roots().await;
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], PathBuf::from("/tmp/root_b"));
        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_adds_and_removes() -> Result<()> {
        let broadcaster = EventBroadcaster::noop()?;
        let manager = ClientManager::new(
            test_config(),
            vec![PathBuf::from("/tmp/root_a"), PathBuf::from("/tmp/root_b")],
            broadcaster,
        );

        // Sync: remove /tmp/root_a, keep /tmp/root_b, add /tmp/root_c
        manager
            .sync_roots(vec![
                PathBuf::from("/tmp/root_b"),
                PathBuf::from("/tmp/root_c"),
            ])
            .await?;

        let roots = manager.roots().await;
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/tmp/root_b"));
        assert_eq!(roots[1], PathBuf::from("/tmp/root_c"));
        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_no_change() -> Result<()> {
        let broadcaster = EventBroadcaster::noop()?;
        let manager = ClientManager::new(
            test_config(),
            vec![PathBuf::from("/tmp/root_a")],
            broadcaster,
        );

        manager
            .sync_roots(vec![PathBuf::from("/tmp/root_a")])
            .await?;

        let roots = manager.roots().await;
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], PathBuf::from("/tmp/root_a"));
        Ok(())
    }
}

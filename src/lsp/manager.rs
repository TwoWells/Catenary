// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

use anyhow::{Result, anyhow};
use ignore::WalkBuilder;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::Config;
use crate::lsp::LspClient;
use crate::lsp::state::ServerStatus;
use crate::session::EventBroadcaster;

/// Manages the lifecycle of LSP clients (spawning, caching, shutdown).
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

    /// Spawns LSP servers for languages detected in the workspace.
    ///
    /// Scans workspace roots for file types, matches against configured
    /// server keys, and only spawns servers for languages actually present.
    /// Servers that fail to spawn are logged and skipped — a misconfigured
    /// server should not prevent other servers from starting.
    pub async fn spawn_all(&self) {
        let roots = self.roots.lock().await.clone();
        let configured_keys: HashSet<&str> =
            self.config.server.keys().map(String::as_str).collect();
        let relevant = detect_workspace_languages(&roots, &configured_keys);

        if relevant.is_empty() {
            info!("No configured languages detected in workspace");
            return;
        }

        let mut sorted: Vec<&str> = relevant.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        info!("Detected languages in workspace: {}", sorted.join(", "));

        for lang in &relevant {
            if let Err(e) = self.get_client(lang).await {
                warn!("Failed to spawn LSP server for {lang}: {e}");
            }
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

        // Notify clients that support dynamic workspace folders,
        // restart those that don't.
        let clients = self.active_clients.lock().await.clone();
        let mut to_restart = Vec::new();
        for (lang, client_mutex) in &clients {
            let client = client_mutex.lock().await;
            if !client.is_alive() {
                continue;
            }
            if client.supports_workspace_folders() {
                if let Err(e) = client
                    .did_change_workspace_folders(vec![folder.clone()], vec![])
                    .await
                {
                    warn!(
                        "Failed to notify {} server about new workspace folder: {}",
                        lang, e
                    );
                }
            } else {
                to_restart.push(lang.clone());
            }
        }

        for lang in &to_restart {
            info!(
                "{} server does not support workspace folder changes, restarting",
                lang
            );
            self.shutdown_client(lang).await;
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

        // Notify clients that support dynamic workspace folders,
        // restart those that don't.
        let clients = self.active_clients.lock().await.clone();
        let mut to_restart = Vec::new();
        for (lang, client_mutex) in &clients {
            let client = client_mutex.lock().await;
            if !client.is_alive() {
                continue;
            }
            if client.supports_workspace_folders() {
                if let Err(e) = client
                    .did_change_workspace_folders(vec![], vec![folder.clone()])
                    .await
                {
                    warn!(
                        "Failed to notify {} server about removed workspace folder: {}",
                        lang, e
                    );
                }
            } else {
                to_restart.push(lang.clone());
            }
        }

        for lang in &to_restart {
            info!(
                "{} server does not support workspace folder changes, restarting",
                lang
            );
            self.shutdown_client(lang).await;
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

        // Notify clients that support dynamic workspace folders,
        // restart those that don't.
        let clients = self.active_clients.lock().await.clone();
        let mut to_restart = Vec::new();
        for (lang, client_mutex) in &clients {
            let client = client_mutex.lock().await;
            if !client.is_alive() {
                continue;
            }
            if client.supports_workspace_folders() {
                if let Err(e) = client
                    .did_change_workspace_folders(added_folders.clone(), removed_folders.clone())
                    .await
                {
                    warn!(
                        "Failed to notify {} server about workspace folder changes: {}",
                        lang, e
                    );
                }
            } else {
                to_restart.push(lang.clone());
            }
        }

        for lang in &to_restart {
            info!(
                "{} server does not support workspace folder changes, restarting",
                lang
            );
            self.shutdown_client(lang).await;
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
        let roots = self.roots.lock().await.clone();
        client
            .initialize(&roots, server_config.initialization_options.clone())
            .await?;

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

/// Scans workspace roots for files and returns the set of configured
/// language keys that have matching files present.
///
/// Respects `.gitignore` and skips hidden files. Exits early once all
/// configured languages have been detected.
#[must_use]
#[allow(clippy::implicit_hasher, reason = "All callers use the default hasher")]
pub fn detect_workspace_languages(
    roots: &[PathBuf],
    configured_keys: &HashSet<&str>,
) -> HashSet<String> {
    let mut detected = HashSet::new();

    for root in roots {
        if !root.exists() {
            continue;
        }

        let walker = WalkBuilder::new(root).git_ignore(true).hidden(true).build();

        for entry in walker.flatten() {
            let path = entry.path();

            // Filename-based detection
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                let lang = match name {
                    "Dockerfile" => Some("dockerfile"),
                    "Makefile" => Some("makefile"),
                    "CMakeLists.txt" => Some("cmake"),
                    _ => None,
                };
                if let Some(l) = lang {
                    if configured_keys.contains(l) {
                        detected.insert(l.to_string());
                    }
                    if detected.len() == configured_keys.len() {
                        return detected;
                    }
                    continue;
                }
            }

            // Extension-based detection
            if let Some(ext) = path.extension().and_then(|e| e.to_str())
                && let Some(lang) = extension_to_config_key(ext)
                && configured_keys.contains(lang)
            {
                detected.insert(lang.to_string());
            }

            if detected.len() == configured_keys.len() {
                return detected;
            }
        }
    }

    detected
}

/// Maps a file extension to the language config key used in
/// `config.server`.
fn extension_to_config_key(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" => Some("rust"),
        "py" => Some("python"),
        "go" => Some("go"),
        "js" | "jsx" => Some("javascript"),
        "ts" | "tsx" => Some("typescript"),
        "c" => Some("c"),
        "cpp" | "cc" | "cxx" | "h" | "hpp" => Some("cpp"),
        "cs" => Some("csharp"),
        "java" => Some("java"),
        "kt" | "kts" => Some("kotlin"),
        "swift" => Some("swift"),
        "rb" => Some("ruby"),
        "php" => Some("php"),
        "sh" | "bash" | "zsh" => Some("shellscript"),
        "json" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "toml" => Some("toml"),
        "md" => Some("markdown"),
        "html" => Some("html"),
        "css" => Some("css"),
        "scss" => Some("scss"),
        "lua" => Some("lua"),
        "sql" => Some("sql"),
        "zig" => Some("zig"),
        "mojo" => Some("mojo"),
        "dart" => Some("dart"),
        "nix" => Some("nix"),
        "proto" => Some("proto"),
        "graphql" | "gql" => Some("graphql"),
        "r" | "R" => Some("r"),
        "jl" => Some("julia"),
        "scala" | "sc" => Some("scala"),
        "hs" => Some("haskell"),
        "ex" | "exs" => Some("elixir"),
        "erl" | "hrl" => Some("erlang"),
        "vim" => Some("vim"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;
    use anyhow::Result;

    fn test_config() -> Config {
        Config {
            server: HashMap::new(),
            idle_timeout: 300,
        }
    }

    /// Locate the mockls binary in the same directory as the test executable.
    /// During `cargo test`, all binaries are built into the same `target/debug/deps`
    /// parent directory.
    fn mockls_bin() -> PathBuf {
        let test_exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .map(|p| p.join("mockls"));
        test_exe.unwrap_or_else(|| PathBuf::from("mockls"))
    }

    fn mockls_config() -> Config {
        let bin = mockls_bin();
        let mut server = HashMap::new();
        server.insert(
            "shellscript".to_string(),
            ServerConfig {
                command: bin.to_string_lossy().to_string(),
                args: vec![],
                initialization_options: None,
            },
        );
        Config {
            server,
            idle_timeout: 300,
        }
    }

    fn mockls_workspace_folders_config() -> Config {
        let bin = mockls_bin();
        let mut server = HashMap::new();
        server.insert(
            "shellscript".to_string(),
            ServerConfig {
                command: bin.to_string_lossy().to_string(),
                args: vec!["--workspace-folders".to_string()],
                initialization_options: None,
            },
        );
        Config {
            server,
            idle_timeout: 300,
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

    #[tokio::test]
    async fn test_sync_roots_shuts_down_unsupported_client() -> Result<()> {
        // mockls without --workspace-folders does NOT advertise workspace folder support.
        // When roots change, the client should be shut down (and lazily respawned).
        let broadcaster = EventBroadcaster::noop()?;
        let manager = ClientManager::new(mockls_config(), vec![PathBuf::from("/tmp")], broadcaster);

        let client = manager.get_client("shellscript").await?;
        assert!(client.lock().await.is_alive());
        assert!(
            !client.lock().await.supports_workspace_folders(),
            "mockls (no flags) should NOT support workspace folders"
        );

        assert!(manager.active_clients().await.contains_key("shellscript"));

        // sync_roots should shut down the unsupported client
        manager
            .sync_roots(vec![PathBuf::from("/tmp"), PathBuf::from("/var")])
            .await?;

        assert!(
            !manager.active_clients().await.contains_key("shellscript"),
            "mockls client should be removed after sync_roots (no workspace folder support)"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_notifies_supported_client() -> Result<()> {
        // mockls with --workspace-folders DOES advertise workspace folder support.
        // When roots change, it should receive a notification instead of being shut down.
        let broadcaster = EventBroadcaster::noop()?;
        let manager = ClientManager::new(
            mockls_workspace_folders_config(),
            vec![PathBuf::from("/tmp")],
            broadcaster,
        );

        let client = manager.get_client("shellscript").await?;
        assert!(client.lock().await.is_alive());
        assert!(
            client.lock().await.supports_workspace_folders(),
            "mockls --workspace-folders should support workspace folders"
        );

        assert!(manager.active_clients().await.contains_key("shellscript"));

        // sync_roots should send notification, NOT shut down the client
        manager
            .sync_roots(vec![PathBuf::from("/tmp"), PathBuf::from("/var")])
            .await?;

        // Client should still be active (not removed)
        assert!(
            manager.active_clients().await.contains_key("shellscript"),
            "mockls client should still be active after sync_roots (workspace folders supported)"
        );

        Ok(())
    }
}

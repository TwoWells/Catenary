// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

use anyhow::{Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::bridge::filesystem_manager::FilesystemManager;
use crate::bridge::{DocumentManager, DocumentNotification};
use crate::config::Config;
use crate::lsp::LspClient;
use crate::lsp::state::ServerStatus;
use crate::session::MessageLog;

/// Manages the lifecycle of LSP clients, document state, and language detection.
///
/// Single authority for LSP server spawning, caching, shutdown, and document
/// lifecycle. Absorbs `DocumentManager` — document open/change tracking and
/// LSP notifications are tightly coupled to server management.
pub struct LspClientManager {
    config: Config,
    roots: Mutex<Vec<PathBuf>>,
    clients: Mutex<HashMap<String, Arc<Mutex<LspClient>>>>,
    message_log: Arc<MessageLog>,
    fs: Arc<FilesystemManager>,
    doc_manager: Mutex<DocumentManager>,
}

impl LspClientManager {
    /// Creates a new `LspClientManager`.
    #[must_use]
    pub fn new(
        config: Config,
        roots: Vec<PathBuf>,
        message_log: Arc<MessageLog>,
        fs: Arc<FilesystemManager>,
        session_id: String,
    ) -> Self {
        Self {
            config,
            roots: Mutex::new(roots),
            clients: Mutex::new(HashMap::new()),
            message_log,
            fs,
            doc_manager: Mutex::new(DocumentManager::new(session_id)),
        }
    }

    /// Returns a reference to the configuration.
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// Returns a reference to the internal document manager.
    ///
    /// Prefer [`ensure_document_open`](Self::ensure_document_open) for the
    /// common case. This lower-level access is for callers that need custom
    /// notification sequencing (e.g., `DiagnosticsServer` snapshots the
    /// diagnostics generation before sending notifications).
    pub const fn doc_manager(&self) -> &Mutex<DocumentManager> {
        &self.doc_manager
    }

    /// Spawns LSP servers for languages detected in the workspace.
    ///
    /// Walks workspace roots (respecting `.gitignore`), classifies files via
    /// [`FilesystemManager`], and spawns servers for configured languages
    /// that have matching files. Servers that fail to spawn are logged and
    /// skipped — a misconfigured server should not prevent others from starting.
    pub async fn spawn_all(&self) {
        let roots = self.roots.lock().await.clone();
        let configured_keys: HashSet<&str> =
            self.config.language.keys().map(String::as_str).collect();
        let relevant = self.fs.detect_workspace_languages(&roots, &configured_keys);

        if relevant.is_empty() {
            info!("No configured languages detected in workspace");
            return;
        }

        let mut sorted: Vec<&str> = relevant.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        info!("Detected languages in workspace: {}", sorted.join(", "));

        for lang in &relevant {
            if let Err(e) = self.get_or_spawn(lang).await {
                warn!("Failed to spawn LSP server for {lang}: {e}");
            }
        }
    }

    /// Returns the current workspace roots.
    pub async fn roots(&self) -> Vec<PathBuf> {
        self.roots.lock().await.clone()
    }

    /// Removes a workspace root and notifies all active LSP clients.
    ///
    /// # Errors
    ///
    /// Returns an error if the root path cannot be converted to a valid URI.
    pub async fn remove_root(&self, root: &Path) -> Result<()> {
        let uri = format!("file://{}", root.display());
        let name = root.file_name().map_or_else(
            || "workspace".to_string(),
            |s| s.to_string_lossy().to_string(),
        );

        self.roots.lock().await.retain(|r| r != root);

        // Notify clients that support dynamic workspace folders,
        // restart those that don't.
        let clients = self.clients.lock().await.clone();
        let mut to_restart = Vec::new();
        for (lang, client_mutex) in &clients {
            let client = client_mutex.lock().await;
            if !client.is_alive() {
                continue;
            }
            if client.supports_workspace_folders() {
                if let Err(e) = client
                    .did_change_workspace_folders(&[], &[(&uri, &name)])
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

        let added_folders: Vec<(String, String)> = to_add
            .iter()
            .map(|root| {
                (
                    format!("file://{}", root.display()),
                    root.file_name().map_or_else(
                        || "workspace".to_string(),
                        |s| s.to_string_lossy().to_string(),
                    ),
                )
            })
            .collect();

        let removed_folders: Vec<(String, String)> = to_remove
            .iter()
            .map(|root| {
                (
                    format!("file://{}", root.display()),
                    root.file_name().map_or_else(
                        || "workspace".to_string(),
                        |s| s.to_string_lossy().to_string(),
                    ),
                )
            })
            .collect();

        // Update internal state
        *self.roots.lock().await = new_roots;

        let added_refs: Vec<(&str, &str)> = added_folders
            .iter()
            .map(|(u, n)| (u.as_str(), n.as_str()))
            .collect();
        let removed_refs: Vec<(&str, &str)> = removed_folders
            .iter()
            .map(|(u, n)| (u.as_str(), n.as_str()))
            .collect();

        // Notify clients that support dynamic workspace folders,
        // restart those that don't.
        let clients = self.clients.lock().await.clone();
        let mut to_restart = Vec::new();
        for (lang, client_mutex) in &clients {
            let client = client_mutex.lock().await;
            if !client.is_alive() {
                continue;
            }
            if client.supports_workspace_folders() {
                if let Err(e) = client
                    .did_change_workspace_folders(&added_refs, &removed_refs)
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

    /// Gets or spawns a client for the given language key.
    ///
    /// Dead clients are left in the map as tombstones — a server that
    /// crashes will not be restarted. Intentional restarts (e.g. after
    /// `sync_roots`) go through [`Self::shutdown_client`] which removes the
    /// entry so a fresh spawn can occur.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The server previously died (tombstone).
    /// - No LSP server is configured for the language.
    /// - The server fails to spawn.
    /// - The server fails to initialize.
    pub async fn get_or_spawn(&self, lang: &str) -> Result<Arc<Mutex<LspClient>>> {
        // Resolve inherit to find the canonical key
        let (canonical, lang_config) = self
            .config
            .resolve_language(lang)
            .ok_or_else(|| anyhow!("No LSP server configured for language '{lang}'"))?;

        // Check if a client already exists under the canonical key
        if let Some(client) = self.clients.lock().await.get(canonical) {
            if client.lock().await.is_alive() {
                return Ok(client.clone());
            }
            anyhow::bail!("LSP server for '{canonical}' is dead");
        }

        let mut clients = self.clients.lock().await;

        // Double-check after acquiring write lock
        if let Some(client) = clients.get(canonical) {
            if client.lock().await.is_alive() {
                return Ok(client.clone());
            }
            anyhow::bail!("LSP server for '{canonical}' is dead");
        }

        let server_name = lang_config
            .servers
            .first()
            .ok_or_else(|| anyhow!("No servers configured for language '{canonical}'"))?;

        let server_def = self
            .config
            .server
            .get(server_name)
            .ok_or_else(|| anyhow!("Server '{server_name}' not found in [server.*] config"))?;

        info!(
            "Spawning LSP server for {}: {} {}",
            canonical,
            server_def.command,
            server_def.args.join(" ")
        );

        let args: Vec<&str> = server_def
            .args
            .iter()
            .map(|s: &String| s.as_str())
            .collect();
        let mut client = LspClient::spawn(
            &server_def.command,
            &args,
            canonical,
            self.message_log.clone(),
            server_def.settings.clone(),
        )?;

        // Initialize
        let roots = self.roots.lock().await.clone();
        client
            .initialize(&roots, server_def.initialization_options.clone())
            .await?;

        let client_mutex = Arc::new(Mutex::new(client));
        clients.insert(canonical.to_string(), client_mutex.clone());
        drop(clients);

        Ok(client_mutex)
    }

    /// Gets a client for a file path, detecting the language automatically.
    ///
    /// Uses [`FilesystemManager`] for language detection (extension, filename,
    /// shebang). Falls back to the raw file extension as a direct config key
    /// for custom or test languages (e.g., `.yX4Za` → config key `"yX4Za"`).
    ///
    /// # Errors
    ///
    /// Returns an error if no language can be detected for the path, or if
    /// the detected language has no configured server, or if the server
    /// fails to spawn.
    pub async fn get_client(&self, path: &Path) -> Result<Arc<Mutex<LspClient>>> {
        // Primary: use FilesystemManager for language detection
        if let Some(lang_id) = self.fs.language_id(path)
            && let Ok(client) = self.get_or_spawn(lang_id).await
        {
            return Ok(client);
        }

        // Fallback: try file extension as direct config key (custom/test languages)
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .ok_or_else(|| anyhow!("No LSP server configured for {}", path.display()))?;
        self.get_or_spawn(ext).await
    }

    /// Ensures a document is open and synced with its LSP server.
    ///
    /// Gets the client for the file's language, opens the document if not
    /// already open, and sends the appropriate `didOpen` or `didChange`
    /// notification. Returns the document URI and client for further LSP
    /// requests.
    ///
    /// # Errors
    ///
    /// Returns an error if language detection fails, the server is dead,
    /// or the document cannot be opened.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "Client lock held across notification send"
    )]
    pub async fn ensure_document_open(
        &self,
        path: &Path,
        parent_id: Option<i64>,
    ) -> Result<(String, Arc<Mutex<LspClient>>)> {
        let client_mutex = self.get_client(path).await?;
        let mut doc_manager = self.doc_manager.lock().await;
        let mut client = client_mutex.lock().await;

        client.set_parent_id(parent_id);

        if !client.is_alive() {
            client.set_parent_id(None);
            return Err(anyhow!(
                "[{}] server is no longer running",
                client.language()
            ));
        }

        let uri = doc_manager.uri_for_path(path)?;

        if let Some(notification) = doc_manager.ensure_open(path).await? {
            match notification {
                DocumentNotification::Open {
                    language_id,
                    version,
                    text,
                    ..
                } => {
                    client.did_open(&uri, &language_id, version, &text).await?;
                }
                DocumentNotification::Change { version, text, .. } => {
                    client.did_change(&uri, version, &text).await?;
                }
            }
        }

        drop(doc_manager);
        drop(client);
        Ok((uri, client_mutex))
    }

    /// Spawns LSP servers for new languages detected in the given file paths.
    ///
    /// Used by workspace-wide tools (grep, glob) to discover languages added
    /// mid-session. For each path, detects the language via
    /// [`FilesystemManager`]. Only spawns servers for configured languages
    /// not already active. Servers that fail to spawn are logged and skipped.
    pub async fn ensure_clients_for_paths(&self, paths: &[PathBuf]) {
        let configured_keys: HashSet<&str> =
            self.config.language.keys().map(String::as_str).collect();

        let mut to_spawn: HashSet<String> = HashSet::new();

        {
            let active = self.clients.lock().await;
            for path in paths {
                let key = self.fs.language_id(path).map(str::to_string).or_else(|| {
                    path.extension()
                        .and_then(|e| e.to_str())
                        .map(str::to_string)
                });

                if let Some(key) = key
                    && configured_keys.contains(key.as_str())
                    && !active.contains_key(&key)
                {
                    to_spawn.insert(key);
                }
            }
        }

        if to_spawn.is_empty() {
            return;
        }

        let mut sorted: Vec<&str> = to_spawn.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        info!("Mid-session server spawn for: {}", sorted.join(", "));

        for lang in &to_spawn {
            if let Err(e) = self.get_or_spawn(lang).await {
                warn!("Failed to spawn LSP server for {lang}: {e}");
            }
        }
    }

    /// Returns a snapshot of all clients (including dead ones).
    pub async fn clients(&self) -> HashMap<String, Arc<Mutex<LspClient>>> {
        self.clients.lock().await.clone()
    }

    /// Returns status of all active servers.
    pub async fn all_server_status(&self) -> Vec<ServerStatus> {
        let clients = self.clients.lock().await.clone();
        let mut statuses = Vec::new();

        for (lang, client_mutex) in clients {
            let status = client_mutex.lock().await.status(lang);
            statuses.push(status);
        }

        statuses
    }

    /// Shuts down a specific client if it exists.
    pub async fn shutdown_client(&self, lang: &str) {
        let mut clients = self.clients.lock().await;
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
    ///
    /// Each server gets 5 seconds to respond to the graceful
    /// `shutdown`/`exit` sequence. Servers that don't respond in time
    /// are dropped, which triggers the `Connection` drop handler to SIGKILL them.
    pub async fn shutdown_all(&self) {
        let mut clients = self.clients.lock().await;
        for (lang, client_mutex) in clients.drain() {
            let mut client = client_mutex.lock().await;
            if client.is_alive() {
                let result = tokio::time::timeout(Duration::from_secs(5), client.shutdown()).await;
                drop(client);
                match result {
                    Ok(Err(e)) => {
                        warn!("Failed to shutdown LSP server for {}: {}", lang, e);
                    }
                    Err(_) => {
                        warn!(
                            "LSP server for {} did not respond to shutdown within 5s, killing",
                            lang
                        );
                    }
                    Ok(Ok(())) => {}
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::config::{IconConfig, LanguageConfig, ServerDef};
    use crate::session::MessageLog;
    use anyhow::Result;

    const MOCK_LANG_A: &str = "yX4Za";

    fn test_message_log() -> Arc<MessageLog> {
        Arc::new(MessageLog::noop())
    }

    fn test_fs() -> Arc<FilesystemManager> {
        Arc::new(FilesystemManager::new())
    }

    fn test_config() -> Config {
        Config {
            language: HashMap::new(),
            server: HashMap::new(),
            idle_timeout: 300,
            log_retention_days: 7,
            icons: IconConfig::default(),
            tui: crate::config::TuiConfig::default(),
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
        let server_name = format!("mockls-{MOCK_LANG_A}");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![MOCK_LANG_A.to_string()],
                initialization_options: None,
                settings: None,
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![server_name],
                min_severity: None,
                inherit: None,
            },
        );
        Config {
            language,
            server,
            idle_timeout: 300,
            log_retention_days: 7,
            icons: IconConfig::default(),
            tui: crate::config::TuiConfig::default(),
        }
    }

    fn mockls_workspace_folders_config() -> Config {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-wf");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![MOCK_LANG_A.to_string(), "--workspace-folders".to_string()],
                initialization_options: None,
                settings: None,
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![server_name],
                min_severity: None,
                inherit: None,
            },
        );
        Config {
            language,
            server,
            idle_timeout: 300,
            log_retention_days: 7,
            icons: IconConfig::default(),
            tui: crate::config::TuiConfig::default(),
        }
    }

    #[tokio::test]
    async fn test_roots_returns_initial_roots() -> Result<()> {
        let manager = LspClientManager::new(
            test_config(),
            vec![PathBuf::from("/tmp/root_a"), PathBuf::from("/tmp/root_b")],
            test_message_log(),
            test_fs(),
            String::new(),
        );

        let roots = manager.roots().await;
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/tmp/root_a"));
        assert_eq!(roots[1], PathBuf::from("/tmp/root_b"));
        Ok(())
    }

    #[tokio::test]
    async fn test_roots_empty_initial() -> Result<()> {
        let manager = LspClientManager::new(
            test_config(),
            vec![],
            test_message_log(),
            test_fs(),
            String::new(),
        );

        assert!(manager.roots().await.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_remove_root() -> Result<()> {
        let manager = LspClientManager::new(
            test_config(),
            vec![PathBuf::from("/tmp/root_a"), PathBuf::from("/tmp/root_b")],
            test_message_log(),
            test_fs(),
            String::new(),
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
        let manager = LspClientManager::new(
            test_config(),
            vec![PathBuf::from("/tmp/root_a"), PathBuf::from("/tmp/root_b")],
            test_message_log(),
            test_fs(),
            String::new(),
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
        let manager = LspClientManager::new(
            test_config(),
            vec![PathBuf::from("/tmp/root_a")],
            test_message_log(),
            test_fs(),
            String::new(),
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
        let manager = LspClientManager::new(
            mockls_config(),
            vec![PathBuf::from("/tmp")],
            test_message_log(),
            test_fs(),
            String::new(),
        );

        let client = manager.get_or_spawn(MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());
        assert!(
            !client.lock().await.supports_workspace_folders(),
            "mockls (no flags) should NOT support workspace folders"
        );

        assert!(manager.clients().await.contains_key(MOCK_LANG_A));

        // sync_roots should shut down the unsupported client
        manager
            .sync_roots(vec![PathBuf::from("/tmp"), PathBuf::from("/var")])
            .await?;

        assert!(
            !manager.clients().await.contains_key(MOCK_LANG_A),
            "mockls client should be removed after sync_roots (no workspace folder support)"
        );

        Ok(())
    }

    /// mockls with `--send-configuration-request` sends a `workspace/configuration`
    /// request with `section: "mockls"` during initialization. This test verifies
    /// that configured settings are threaded through to the response handler.
    #[tokio::test]
    async fn test_configuration_returns_settings() -> Result<()> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-cfg");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![
                    MOCK_LANG_A.to_string(),
                    "--send-configuration-request".to_string(),
                ],
                initialization_options: None,
                settings: Some(serde_json::json!({"mockls": {"key": "value"}})),
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![server_name],
                min_severity: None,
                inherit: None,
            },
        );
        let config = Config {
            language,
            server,
            idle_timeout: 300,
            log_retention_days: 7,
            icons: IconConfig::default(),
            tui: crate::config::TuiConfig::default(),
        };

        let manager = LspClientManager::new(
            config,
            vec![PathBuf::from("/tmp")],
            test_message_log(),
            test_fs(),
            String::new(),
        );

        // get_client spawns + initializes; mockls sends workspace/configuration
        // during init. If Catenary responds correctly, initialization succeeds.
        let client = manager.get_or_spawn(MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_notifies_supported_client() -> Result<()> {
        // mockls with --workspace-folders DOES advertise workspace folder support.
        // When roots change, it should receive a notification instead of being shut down.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            vec![PathBuf::from("/tmp")],
            test_message_log(),
            test_fs(),
            String::new(),
        );

        let client = manager.get_or_spawn(MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());
        assert!(
            client.lock().await.supports_workspace_folders(),
            "mockls --workspace-folders should support workspace folders"
        );

        assert!(manager.clients().await.contains_key(MOCK_LANG_A));

        // sync_roots should send notification, NOT shut down the client
        manager
            .sync_roots(vec![PathBuf::from("/tmp"), PathBuf::from("/var")])
            .await?;

        // Client should still be active (not removed)
        assert!(
            manager.clients().await.contains_key(MOCK_LANG_A),
            "mockls client should still be active after sync_roots (workspace folders supported)"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_for_paths_spawns_new_language() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            vec![PathBuf::from("/tmp")],
            test_message_log(),
            test_fs(),
            String::new(),
        );

        assert!(manager.clients().await.is_empty());

        // A file with the mock language extension triggers a spawn
        let paths = vec![PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"))];
        manager.ensure_clients_for_paths(&paths).await;

        assert!(
            manager.clients().await.contains_key(MOCK_LANG_A),
            "ensure_clients_for_paths should spawn the mock language server"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_for_paths_skips_existing() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            vec![PathBuf::from("/tmp")],
            test_message_log(),
            test_fs(),
            String::new(),
        );

        // Pre-spawn the server
        let _ = manager.get_or_spawn(MOCK_LANG_A).await?;
        assert_eq!(manager.clients().await.len(), 1);

        // ensure_clients_for_paths should not fail or double-spawn
        let paths = vec![PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"))];
        manager.ensure_clients_for_paths(&paths).await;

        assert_eq!(
            manager.clients().await.len(),
            1,
            "should not create a duplicate client"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_for_paths_ignores_unconfigured() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            vec![PathBuf::from("/tmp")],
            test_message_log(),
            test_fs(),
            String::new(),
        );

        // .xyz has no configured server — should be silently skipped
        let paths = vec![PathBuf::from("/tmp/test.xyz")];
        manager.ensure_clients_for_paths(&paths).await;

        assert!(
            manager.clients().await.is_empty(),
            "unconfigured languages should not trigger a spawn"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_get_client_resolves_language_from_path() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            vec![PathBuf::from("/tmp")],
            test_message_log(),
            test_fs(),
            String::new(),
        );

        // A file with the mock language extension should resolve to the mock server
        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        let client = manager.get_client(&path).await?;
        assert!(client.lock().await.is_alive());
        Ok(())
    }

    #[tokio::test]
    async fn test_get_client_unknown_language_errors() {
        let manager = LspClientManager::new(
            mockls_config(),
            vec![PathBuf::from("/tmp")],
            test_message_log(),
            test_fs(),
            String::new(),
        );

        // A file with an unknown extension and no config key should error
        let result = manager.get_client(Path::new("/tmp/test.xyz")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_ensure_document_open_sends_did_open() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            vec![PathBuf::from("/tmp")],
            test_message_log(),
            test_fs(),
            String::new(),
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(format!("test.{MOCK_LANG_A}"));
        std::fs::write(&path, "content").expect("write");

        let (uri, client_mutex) = manager.ensure_document_open(&path, None).await?;
        assert!(uri.starts_with("file://"));
        assert!(client_mutex.lock().await.is_alive());
        Ok(())
    }
}

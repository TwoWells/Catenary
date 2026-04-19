// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

use anyhow::{Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::bridge::DocumentManager;
use crate::bridge::filesystem_manager::FilesystemManager;
use crate::config::Config;
use crate::logging::LoggingServer;
use crate::lsp::LspClient;
use crate::lsp::glob::{FileChange, GlobPattern, WatchKind};
use crate::lsp::state::ServerStatus;

/// Filters filesystem changes against a server's watcher registrations.
///
/// Returns `(uri, FileChangeType as u8)` pairs for changes that match at
/// least one watcher's glob pattern and watch kind.
fn match_file_changes(
    changes: &[FileChange],
    watchers: &[(GlobPattern, WatchKind)],
    roots: &[PathBuf],
) -> Vec<(String, u8)> {
    changes
        .iter()
        .filter(|change| {
            watchers.iter().any(|(pattern, kind)| {
                pattern.is_match(&change.path, roots) && kind.matches(change.change_type)
            })
        })
        .map(|change| {
            (
                format!("file://{}", change.path.display()),
                change.change_type as u8,
            )
        })
        .collect()
}

/// Manages the lifecycle of LSP clients, document state, and language detection.
///
/// Single authority for LSP server spawning, caching, shutdown, and document
/// lifecycle. Absorbs `DocumentManager` — document open/change tracking and
/// LSP notifications are tightly coupled to server management.
pub struct LspClientManager {
    config: Config,
    roots: Mutex<Vec<PathBuf>>,
    clients: Mutex<HashMap<String, Arc<Mutex<LspClient>>>>,
    logging: LoggingServer,
    fs: Arc<FilesystemManager>,
    doc_manager: Mutex<DocumentManager>,
}

impl LspClientManager {
    /// Creates a new `LspClientManager`.
    #[must_use]
    pub fn new(
        config: Config,
        roots: Vec<PathBuf>,
        logging: LoggingServer,
        fs: Arc<FilesystemManager>,
    ) -> Self {
        Self {
            config,
            roots: Mutex::new(roots),
            clients: Mutex::new(HashMap::new()),
            logging,
            fs,
            doc_manager: Mutex::new(DocumentManager::new()),
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
                warn!(
                    source = "lsp.lifecycle",
                    language = lang.as_str(),
                    "Failed to spawn LSP server for {lang}: {e}",
                );
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
                    info!(
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
                    info!(
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
        let lang_config = self
            .config
            .resolve_language(lang)
            .ok_or_else(|| anyhow!("No LSP server configured for language '{lang}'"))?;

        // Check if a client already exists
        if let Some(client) = self.clients.lock().await.get(lang) {
            if client.lock().await.is_alive() {
                return Ok(client.clone());
            }
            anyhow::bail!("LSP server for '{lang}' is dead");
        }

        let mut clients = self.clients.lock().await;

        // Double-check after acquiring write lock
        if let Some(client) = clients.get(lang) {
            if client.lock().await.is_alive() {
                return Ok(client.clone());
            }
            anyhow::bail!("LSP server for '{lang}' is dead");
        }

        let server_name = &lang_config
            .servers
            .first()
            .ok_or_else(|| anyhow!("No servers configured for language '{lang}'"))?
            .name;

        let server_def = self
            .config
            .server
            .get(server_name)
            .ok_or_else(|| anyhow!("Server '{server_name}' not found in [server.*] config"))?;

        info!(
            "Spawning LSP server for {}: {} {}",
            lang,
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
            lang,
            self.logging.clone(),
            server_def.settings.clone(),
        )?;

        // Initialize
        let roots = self.roots.lock().await.clone();
        client
            .initialize(&roots, server_def.initialization_options.clone())
            .await?;

        let client_mutex = Arc::new(Mutex::new(client));
        clients.insert(lang.to_string(), client_mutex.clone());
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

    /// Closes a document previously opened via [`ensure_document_open`](Self::ensure_document_open).
    ///
    /// Decrements the ref count and sends `didClose` when the count reaches
    /// zero. Safe to call even if the document was already closed — the
    /// ref-counted [`DocumentManager`] handles this gracefully.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "Lock ordering: doc_manager then client — both needed for close"
    )]
    pub async fn close_document(&self, uri: &str, client: &Arc<Mutex<LspClient>>) {
        let mut dm = self.doc_manager.lock().await;
        if dm.close(uri) {
            drop(dm);
            let _ = client.lock().await.did_close(uri).await;
        }
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
        let canonical = path.canonicalize()?;
        let text = tokio::fs::read_to_string(&canonical).await?;

        let (first_open, version) = doc_manager.open(&uri);
        if first_open {
            let language_id = self.fs.language_id(path).unwrap_or("plaintext").to_string();
            client.did_open(&uri, &language_id, version, &text).await?;
        } else {
            client.did_change(&uri, version, &text).await?;
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
                warn!(
                    source = "lsp.lifecycle",
                    language = lang.as_str(),
                    "Failed to spawn LSP server for {lang}: {e}",
                );
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
                info!("Failed to shutdown LSP server for {}: {}", lang, e);
            }
        }
    }

    /// Diffs the filesystem and notifies servers with matching file watcher
    /// registrations.
    ///
    /// Called before each LSP interaction (tool call, diagnostics pipeline).
    /// No-op if no changes are detected or no servers have registrations.
    pub async fn notify_file_changes(&self) {
        let changes = self.fs.diff();
        if changes.is_empty() {
            return;
        }

        let roots = self.roots.lock().await.clone();
        let clients = self.clients.lock().await.clone();

        let mut notified = 0u32;
        for (lang, client_mutex) in &clients {
            let client = client_mutex.lock().await;
            if !client.is_alive() {
                continue;
            }

            let watchers = client.server().file_watcher_snapshot();
            if watchers.is_empty() {
                continue;
            }

            let matched = match_file_changes(&changes, &watchers, &roots);
            if matched.is_empty() {
                continue;
            }

            let refs: Vec<(&str, u8)> = matched.iter().map(|(u, t)| (u.as_str(), *t)).collect();
            if let Err(e) = client.did_change_watched_files(&refs).await {
                info!("Failed to send didChangeWatchedFiles to {lang}: {e}");
            }
            drop(client);
            notified += 1;
        }

        debug!(
            "{} filesystem changes, {} servers notified",
            changes.len(),
            notified
        );
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
                        info!("Failed to shutdown LSP server for {}: {}", lang, e);
                    }
                    Err(_) => {
                        info!(
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
    use crate::config::{LanguageConfig, ServerBinding, ServerDef};
    use anyhow::Result;

    const MOCK_LANG_A: &str = "yX4Za";

    fn test_logging() -> LoggingServer {
        LoggingServer::new()
    }

    fn test_fs() -> Arc<FilesystemManager> {
        Arc::new(FilesystemManager::new())
    }

    fn test_config() -> Config {
        Config {
            language: HashMap::new(),
            server: HashMap::new(),
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
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
                servers: vec![ServerBinding::new(server_name)],
                ..LanguageConfig::default()
            },
        );
        Config {
            language,
            server,
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
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
                servers: vec![ServerBinding::new(server_name)],
                ..LanguageConfig::default()
            },
        );
        Config {
            language,
            server,
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
        }
    }

    #[tokio::test]
    async fn test_roots_returns_initial_roots() -> Result<()> {
        let manager = LspClientManager::new(
            test_config(),
            vec![PathBuf::from("/tmp/root_a"), PathBuf::from("/tmp/root_b")],
            test_logging(),
            test_fs(),
        );

        let roots = manager.roots().await;
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/tmp/root_a"));
        assert_eq!(roots[1], PathBuf::from("/tmp/root_b"));
        Ok(())
    }

    #[tokio::test]
    async fn test_roots_empty_initial() -> Result<()> {
        let manager = LspClientManager::new(test_config(), vec![], test_logging(), test_fs());

        assert!(manager.roots().await.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_remove_root() -> Result<()> {
        let manager = LspClientManager::new(
            test_config(),
            vec![PathBuf::from("/tmp/root_a"), PathBuf::from("/tmp/root_b")],
            test_logging(),
            test_fs(),
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
            test_logging(),
            test_fs(),
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
            test_logging(),
            test_fs(),
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
            test_logging(),
            test_fs(),
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
                servers: vec![ServerBinding::new(server_name)],
                ..LanguageConfig::default()
            },
        );
        let config = Config {
            language,
            server,
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
        };

        let manager = LspClientManager::new(
            config,
            vec![PathBuf::from("/tmp")],
            test_logging(),
            test_fs(),
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
            test_logging(),
            test_fs(),
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
            test_logging(),
            test_fs(),
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
            test_logging(),
            test_fs(),
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
            test_logging(),
            test_fs(),
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
            test_logging(),
            test_fs(),
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
            test_logging(),
            test_fs(),
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
            test_logging(),
            test_fs(),
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(format!("test.{MOCK_LANG_A}"));
        std::fs::write(&path, "content").expect("write");

        let (uri, client_mutex) = manager.ensure_document_open(&path, None).await?;
        assert!(uri.starts_with("file://"));
        assert!(client_mutex.lock().await.is_alive());
        Ok(())
    }

    // --- match_file_changes ---

    mod file_change_matching {
        use super::*;
        use crate::lsp::glob::{FileChange, FileChangeType, GlobPattern, WatchKind};

        fn plain_glob(pattern: &str) -> GlobPattern {
            GlobPattern::from_value(&serde_json::json!(pattern)).expect("valid glob")
        }

        #[test]
        fn matches_created_file() {
            let changes = vec![FileChange {
                path: PathBuf::from("/project/src/new.rs"),
                change_type: FileChangeType::Created,
            }];
            let watchers = vec![(plain_glob("**/*.rs"), WatchKind::from_value(None))];
            let roots = vec![PathBuf::from("/project")];

            let matched = match_file_changes(&changes, &watchers, &roots);
            assert_eq!(matched.len(), 1);
            assert_eq!(matched[0].0, "file:///project/src/new.rs");
            assert_eq!(matched[0].1, FileChangeType::Created as u8);
        }

        #[test]
        fn skips_non_matching_extension() {
            let changes = vec![FileChange {
                path: PathBuf::from("/project/src/file.rs"),
                change_type: FileChangeType::Created,
            }];
            let watchers = vec![(plain_glob("**/*.ts"), WatchKind::from_value(None))];
            let roots = vec![PathBuf::from("/project")];

            let matched = match_file_changes(&changes, &watchers, &roots);
            assert!(matched.is_empty());
        }

        #[test]
        fn empty_changes_returns_empty() {
            let watchers = vec![(plain_glob("**/*.rs"), WatchKind::from_value(None))];
            let roots = vec![PathBuf::from("/project")];

            let matched = match_file_changes(&[], &watchers, &roots);
            assert!(matched.is_empty());
        }

        #[test]
        fn empty_watchers_returns_empty() {
            let changes = vec![FileChange {
                path: PathBuf::from("/project/src/file.rs"),
                change_type: FileChangeType::Created,
            }];
            let roots = vec![PathBuf::from("/project")];

            let matched = match_file_changes(&changes, &[], &roots);
            assert!(matched.is_empty());
        }

        #[test]
        fn batches_multiple_events() {
            let changes = vec![
                FileChange {
                    path: PathBuf::from("/project/src/a.rs"),
                    change_type: FileChangeType::Created,
                },
                FileChange {
                    path: PathBuf::from("/project/src/b.rs"),
                    change_type: FileChangeType::Changed,
                },
                FileChange {
                    path: PathBuf::from("/project/src/c.rs"),
                    change_type: FileChangeType::Deleted,
                },
            ];
            let watchers = vec![(plain_glob("**/*.rs"), WatchKind::from_value(None))];
            let roots = vec![PathBuf::from("/project")];

            let matched = match_file_changes(&changes, &watchers, &roots);
            assert_eq!(matched.len(), 3);
            assert_eq!(matched[0].1, FileChangeType::Created as u8);
            assert_eq!(matched[1].1, FileChangeType::Changed as u8);
            assert_eq!(matched[2].1, FileChangeType::Deleted as u8);
        }

        #[test]
        fn respects_watch_kind_create_only() {
            let changes = vec![
                FileChange {
                    path: PathBuf::from("/project/src/new.rs"),
                    change_type: FileChangeType::Created,
                },
                FileChange {
                    path: PathBuf::from("/project/src/mod.rs"),
                    change_type: FileChangeType::Changed,
                },
                FileChange {
                    path: PathBuf::from("/project/src/old.rs"),
                    change_type: FileChangeType::Deleted,
                },
            ];
            let watchers = vec![(
                plain_glob("**/*.rs"),
                WatchKind::from_value(Some(WatchKind::CREATE)),
            )];
            let roots = vec![PathBuf::from("/project")];

            let matched = match_file_changes(&changes, &watchers, &roots);
            assert_eq!(matched.len(), 1);
            assert_eq!(matched[0].1, FileChangeType::Created as u8);
        }

        #[test]
        fn respects_watch_kind_delete_only() {
            let changes = vec![
                FileChange {
                    path: PathBuf::from("/project/src/new.rs"),
                    change_type: FileChangeType::Created,
                },
                FileChange {
                    path: PathBuf::from("/project/src/old.rs"),
                    change_type: FileChangeType::Deleted,
                },
            ];
            let watchers = vec![(
                plain_glob("**/*.rs"),
                WatchKind::from_value(Some(WatchKind::DELETE)),
            )];
            let roots = vec![PathBuf::from("/project")];

            let matched = match_file_changes(&changes, &watchers, &roots);
            assert_eq!(matched.len(), 1);
            assert_eq!(matched[0].0, "file:///project/src/old.rs");
        }

        #[test]
        fn path_outside_root_not_matched() {
            let changes = vec![FileChange {
                path: PathBuf::from("/other/src/file.rs"),
                change_type: FileChangeType::Created,
            }];
            let watchers = vec![(plain_glob("**/*.rs"), WatchKind::from_value(None))];
            let roots = vec![PathBuf::from("/project")];

            let matched = match_file_changes(&changes, &watchers, &roots);
            assert!(matched.is_empty());
        }

        #[test]
        fn multiple_watchers_any_match() {
            let changes = vec![FileChange {
                path: PathBuf::from("/project/Cargo.toml"),
                change_type: FileChangeType::Changed,
            }];
            let watchers = vec![
                (plain_glob("**/*.rs"), WatchKind::from_value(None)),
                (plain_glob("**/*.toml"), WatchKind::from_value(None)),
            ];
            let roots = vec![PathBuf::from("/project")];

            let matched = match_file_changes(&changes, &watchers, &roots);
            assert_eq!(matched.len(), 1);
        }
    }
}

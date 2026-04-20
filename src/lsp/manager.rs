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
use crate::lsp::instance_key::{InstanceKey, Scope};
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

/// Looks up an existing client instance by trying both `Scope::Workspace`
/// and `Scope::Root(root)` keys. Returns `None` if no instance matches.
fn find_instance(
    clients: &HashMap<InstanceKey, Arc<Mutex<LspClient>>>,
    lang: &str,
    server_name: &str,
    root: &Path,
) -> Option<Arc<Mutex<LspClient>>> {
    let workspace_key =
        InstanceKey::new(lang.to_string(), server_name.to_string(), Scope::Workspace);
    if let Some(client) = clients.get(&workspace_key) {
        return Some(client.clone());
    }
    let root_key = InstanceKey::new(
        lang.to_string(),
        server_name.to_string(),
        Scope::Root(root.to_path_buf()),
    );
    clients.get(&root_key).cloned()
}

/// Manages the lifecycle of LSP clients, document state, and language detection.
///
/// Single authority for LSP server spawning, caching, shutdown, and document
/// lifecycle. Absorbs `DocumentManager` — document open/change tracking and
/// LSP notifications are tightly coupled to server management.
pub struct LspClientManager {
    config: Config,
    clients: Mutex<HashMap<InstanceKey, Arc<Mutex<LspClient>>>>,
    logging: LoggingServer,
    fs: Arc<FilesystemManager>,
    doc_manager: Mutex<DocumentManager>,
}

impl LspClientManager {
    /// Creates a new `LspClientManager`.
    ///
    /// Workspace roots are sourced from the shared [`FilesystemManager`] —
    /// call [`FilesystemManager::set_roots`] before constructing this manager.
    #[must_use]
    pub fn new(config: Config, logging: LoggingServer, fs: Arc<FilesystemManager>) -> Self {
        Self {
            config,
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
    ///
    /// For workspace-capable servers, spawns a single instance (all roots
    /// are already included in the `initialize` request). For legacy
    /// servers, spawns a separate `Scope::Root` instance per root.
    pub async fn spawn_all(&self) {
        let roots = self.fs.roots();
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
            // Spawn the first instance (uses first root).
            let first_client = match self.ensure_server_for_language(lang).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(
                        source = "lsp.lifecycle",
                        language = lang.as_str(),
                        "Failed to spawn LSP server for {lang}: {e}",
                    );
                    continue;
                }
            };

            if roots.len() <= 1 {
                continue;
            }

            let key = first_client.lock().await.server().key();
            let Some(key) = key else { continue };

            // Workspace-capable servers already received all roots in the
            // `initialize` request — no additional notification needed.
            // Legacy servers need a separate instance per remaining root.
            if let Scope::Root(_) = key.scope {
                let server_name = key.server.clone();
                info!(
                    source = "lsp.lifecycle",
                    language = lang.as_str(),
                    server = server_name.as_str(),
                    "Server does not support workspaceFolders — spawning per-root instances",
                );
                for root in &roots[1..] {
                    if let Err(e) = self.spawn(&server_name, lang, root).await {
                        warn!(
                            source = "lsp.lifecycle",
                            language = lang.as_str(),
                            "Failed to spawn per-root instance for {lang} at {}: {e}",
                            root.display(),
                        );
                    }
                }
            }
        }
    }

    /// Returns the current workspace roots.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.fs.roots()
    }

    /// Removes a workspace root and updates all active LSP clients.
    ///
    /// Workspace-capable servers receive a `didChangeWorkspaceFolders`
    /// removal notification. Legacy servers with a `Scope::Root` matching
    /// the removed root are shut down and removed from the map.
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

        let mut roots = self.fs.roots();
        roots.retain(|r| r != root);
        self.fs.set_roots(roots);

        // Notify workspace-capable servers about the removal.
        let clients = self.clients.lock().await.clone();
        for (key, client_mutex) in &clients {
            let client = client_mutex.lock().await;
            if !client.is_alive() || !client.supports_workspace_folders() {
                continue;
            }
            if let Err(e) = client
                .did_change_workspace_folders(&[], &[(&uri, &name)])
                .await
            {
                info!(
                    "Failed to notify {} server about removed workspace folder: {}",
                    key.language_id, e
                );
            }
        }

        // Shut down legacy per-root instances bound to the removed root.
        self.shutdown_root_instances(root).await;

        Ok(())
    }

    /// Synchronizes workspace roots with a new set.
    ///
    /// Diffs against current roots: adds new ones, removes stale ones.
    /// Workspace-capable servers receive a single `didChangeWorkspaceFolders`
    /// notification with both additions and removals. Legacy servers get
    /// per-root instance lifecycle: removed roots have their instances shut
    /// down; added roots get new `Scope::Root` instances spawned (only for
    /// languages that already have active legacy instances).
    ///
    /// # Errors
    ///
    /// Returns an error if any root path cannot be converted to a valid URI.
    pub async fn sync_roots(&self, new_roots: Vec<PathBuf>) -> Result<()> {
        // Snapshot before updating so the diff is computed against old state.
        let current_roots = self.fs.roots();
        self.fs.set_roots(new_roots.clone());

        let to_add: Vec<PathBuf> = new_roots
            .iter()
            .filter(|r| !current_roots.contains(r))
            .cloned()
            .collect();
        let to_remove: Vec<PathBuf> = current_roots
            .iter()
            .filter(|r| !new_roots.contains(r))
            .cloned()
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

        let added_refs: Vec<(&str, &str)> = added_folders
            .iter()
            .map(|(u, n)| (u.as_str(), n.as_str()))
            .collect();
        let removed_refs: Vec<(&str, &str)> = removed_folders
            .iter()
            .map(|(u, n)| (u.as_str(), n.as_str()))
            .collect();

        // Notify workspace-capable servers about all additions and removals.
        let clients = self.clients.lock().await.clone();
        for (key, client_mutex) in &clients {
            let client = client_mutex.lock().await;
            if !client.is_alive() || !client.supports_workspace_folders() {
                continue;
            }
            if let Err(e) = client
                .did_change_workspace_folders(&added_refs, &removed_refs)
                .await
            {
                info!(
                    "Failed to notify {} server about workspace folder changes: {}",
                    key.language_id, e
                );
            }
        }

        // Legacy per-root lifecycle: shut down instances for removed roots.
        for removed in &to_remove {
            self.shutdown_root_instances(removed).await;
        }

        // Legacy per-root lifecycle: spawn instances for added roots.
        // Only for languages that already have active Scope::Root instances.
        if !to_add.is_empty() {
            let add_refs: Vec<&PathBuf> = to_add.iter().collect();
            self.spawn_legacy_for_added_roots(&add_refs).await;
        }

        Ok(())
    }

    /// Pure map lookup by key.
    #[allow(dead_code, reason = "public API for Phase 1c dispatch")]
    async fn get(&self, key: &InstanceKey) -> Option<Arc<Mutex<LspClient>>> {
        self.clients.lock().await.get(key).cloned()
    }

    /// Spawns a server process, runs `initialize`, constructs the final
    /// `InstanceKey` from discovered capabilities, and inserts into the
    /// clients map.
    ///
    /// Uses capability-driven scope: servers that advertise workspace
    /// folder support get `Scope::Workspace`; others get
    /// `Scope::Root(root)`.
    ///
    /// Holds the clients lock across the entire spawn+init sequence to
    /// prevent double-spawns. Pre-fetches roots before acquiring the
    /// clients lock to avoid lock ordering issues.
    ///
    /// # Errors
    ///
    /// Returns an error if the server fails to spawn or initialize.
    async fn spawn(
        &self,
        server_name: &str,
        lang: &str,
        root: &Path,
    ) -> Result<(InstanceKey, Arc<Mutex<LspClient>>)> {
        let server_def = self
            .config
            .server
            .get(server_name)
            .ok_or_else(|| anyhow!("Server '{server_name}' not found in [server.*] config"))?;

        let roots = self.fs.roots();
        let mut clients = self.clients.lock().await;

        // Double-check: another task may have spawned this server
        // while we waited.
        if let Some(found) = find_instance(&clients, lang, server_name, root) {
            if found.lock().await.is_alive() {
                let key = found
                    .lock()
                    .await
                    .server()
                    .key()
                    .ok_or_else(|| anyhow!("Existing server missing instance key"))?;
                return Ok((key, found));
            }
            anyhow::bail!("LSP server '{server_name}' ({lang}) is dead");
        }

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
            server_name,
            self.logging.clone(),
            server_def.settings.clone(),
        )?;

        client
            .initialize(&roots, server_def.initialization_options.clone())
            .await?;

        // Determine scope from capabilities (Option B from §16).
        let scope = if client.supports_workspace_folders() {
            Scope::Workspace
        } else {
            Scope::Root(root.to_path_buf())
        };
        client.server().set_scope(scope);

        let key = client
            .server()
            .key()
            .ok_or_else(|| anyhow!("Failed to construct instance key after initialization"))?;

        let client_mutex = Arc::new(Mutex::new(client));
        clients.insert(key.clone(), client_mutex.clone());
        drop(clients);

        Ok((key, client_mutex))
    }

    /// Get-then-spawn composition.
    ///
    /// Looks up an existing instance by trying both `Scope::Workspace` and
    /// `Scope::Root(root)` keys. On miss, calls [`Self::spawn`]. Dead
    /// servers are left as tombstones — a server that crashes will not be
    /// restarted. Intentional restarts (e.g. after `sync_roots`) go through
    /// [`Self::shutdown_instance`] which removes the entry so a fresh spawn
    /// can occur.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The server previously died (tombstone).
    /// - The server definition is missing from config.
    /// - The server fails to spawn or initialize.
    async fn ensure_server(
        &self,
        lang: &str,
        server_name: &str,
        root: &Path,
    ) -> Result<Arc<Mutex<LspClient>>> {
        // Fast path: check both possible scope keys.
        {
            let clients = self.clients.lock().await;
            if let Some(found) = find_instance(&clients, lang, server_name, root) {
                if found.lock().await.is_alive() {
                    return Ok(found);
                }
                anyhow::bail!("LSP server '{server_name}' ({lang}) is dead");
            }
        }

        // Miss — spawn (spawn handles its own double-check).
        let (_key, client) = self.spawn(server_name, lang, root).await?;
        Ok(client)
    }

    /// Ensures a server is running for the given language.
    ///
    /// Uses the first server from the language's binding list and spawns
    /// with the first workspace root. Single-server-per-language convenience
    /// — Phase 1c callers use [`Self::ensure_server`] directly.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No LSP server is configured for the language.
    /// - No servers are listed in the language binding.
    /// - No workspace roots are available.
    /// - The server previously died (tombstone).
    /// - The server fails to spawn or initialize.
    pub async fn ensure_server_for_language(&self, lang: &str) -> Result<Arc<Mutex<LspClient>>> {
        let lang_config = self
            .config
            .resolve_language(lang)
            .ok_or_else(|| anyhow!("No LSP server configured for language '{lang}'"))?;

        let server_name = &lang_config
            .servers
            .first()
            .ok_or_else(|| anyhow!("No servers configured for language '{lang}'"))?
            .name;

        let root = self
            .fs
            .roots()
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("No workspace roots available for spawning '{lang}'"))?;

        self.ensure_server(lang, server_name, &root).await
    }

    /// Gets a client for a file path, detecting the language automatically.
    ///
    /// Uses [`FilesystemManager`] for language detection (extension, filename,
    /// shebang) and [`FilesystemManager::resolve_root`] for scope-aware
    /// instance lookup. Falls back to the raw file extension as a direct
    /// config key for custom or test languages (e.g., `.yX4Za` → config key
    /// `"yX4Za"`).
    ///
    /// Files outside all workspace roots return an explicit error — the agent
    /// can use `/add-dir` to add the root. Tier 3 (single-file) support is
    /// tracked in misc 28b.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No language can be detected for the path.
    /// - The file is outside all workspace roots.
    /// - The detected language has no configured server.
    /// - The server fails to spawn.
    pub async fn get_client(&self, path: &Path) -> Result<Arc<Mutex<LspClient>>> {
        // Detect language: primary (FilesystemManager) then fallback (raw extension).
        let lang_id = self
            .fs
            .language_id(path)
            .or_else(|| {
                path.extension()
                    .and_then(|e| e.to_str())
                    .map(str::to_string)
            })
            .ok_or_else(|| anyhow!("No LSP server configured for {}", path.display()))?;

        // Resolve owning workspace root.
        let root = self
            .fs
            .resolve_root(path)
            .ok_or_else(|| anyhow!("File outside all workspace roots: {}", path.display()))?;

        // Look up first server binding for the language.
        let lang_config = self
            .config
            .resolve_language(&lang_id)
            .ok_or_else(|| anyhow!("No LSP server configured for language '{lang_id}'"))?;
        let server_name = &lang_config
            .servers
            .first()
            .ok_or_else(|| anyhow!("No servers configured for language '{lang_id}'"))?
            .name;

        // Try existing instances: workspace-scoped first, then root-scoped.
        {
            let clients = self.clients.lock().await;
            if let Some(found) = find_instance(&clients, &lang_id, server_name, &root) {
                if found.lock().await.is_alive() {
                    return Ok(found);
                }
                anyhow::bail!("LSP server '{server_name}' ({lang_id}) is dead");
            }
        }

        // No instance found — spawn via ensure_server.
        self.ensure_server(&lang_id, server_name, &root).await
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
            let language_id = self
                .fs
                .language_id(path)
                .unwrap_or_else(|| "plaintext".to_string());
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
    /// [`FilesystemManager`] and resolves the owning root. Only spawns
    /// servers for configured languages that don't already have an instance
    /// covering the file's root. Unrooted files are skipped. Servers that
    /// fail to spawn are logged and skipped.
    pub async fn ensure_clients_for_paths(&self, paths: &[PathBuf]) {
        let configured_keys: HashSet<&str> =
            self.config.language.keys().map(String::as_str).collect();

        // Collect (language, server_name, root) triples that need spawning.
        let mut to_spawn: HashSet<(String, String, PathBuf)> = HashSet::new();

        {
            let active = self.clients.lock().await;
            for path in paths {
                let lang = self.fs.language_id(path).or_else(|| {
                    path.extension()
                        .and_then(|e| e.to_str())
                        .map(str::to_string)
                });

                let Some(lang) = lang else { continue };
                if !configured_keys.contains(lang.as_str()) {
                    continue;
                }

                // Skip unrooted files.
                let Some(root) = self.fs.resolve_root(path) else {
                    continue;
                };

                let Some(lang_config) = self.config.resolve_language(&lang) else {
                    continue;
                };
                let Some(binding) = lang_config.servers.first() else {
                    continue;
                };

                // Check if a matching instance already exists for this root.
                if find_instance(&active, &lang, &binding.name, &root).is_none() {
                    to_spawn.insert((lang, binding.name.clone(), root));
                }
            }
        }

        if to_spawn.is_empty() {
            return;
        }

        let mut sorted: Vec<&str> = to_spawn.iter().map(|(l, _, _)| l.as_str()).collect();
        sorted.sort_unstable();
        sorted.dedup();
        info!("Mid-session server spawn for: {}", sorted.join(", "));

        for (lang, server_name, root) in &to_spawn {
            if let Err(e) = self.ensure_server(lang, server_name, root).await {
                warn!(
                    source = "lsp.lifecycle",
                    language = lang.as_str(),
                    "Failed to spawn LSP server for {lang}: {e}",
                );
            }
        }
    }

    /// Returns a snapshot of all clients (including dead ones).
    pub async fn clients(&self) -> HashMap<InstanceKey, Arc<Mutex<LspClient>>> {
        self.clients.lock().await.clone()
    }

    /// Returns status of all active servers.
    pub async fn all_server_status(&self) -> Vec<ServerStatus> {
        let clients = self.clients.lock().await.clone();
        let mut statuses = Vec::new();

        for (key, client_mutex) in &clients {
            let status = client_mutex.lock().await.status(key);
            statuses.push(status);
        }

        statuses
    }

    /// Shuts down a specific server instance if it exists.
    pub async fn shutdown_instance(&self, key: &InstanceKey) {
        let mut clients = self.clients.lock().await;
        if let Some(client_mutex) = clients.remove(key) {
            info!("Shutting down LSP server instance {}", key);
            let mut client = client_mutex.lock().await;
            if client.is_alive()
                && let Err(e) = client.shutdown().await
            {
                info!("Failed to shutdown LSP server instance {}: {}", key, e);
            }
        }
    }

    /// Shuts down all instances bound to a specific root.
    ///
    /// Only affects `Scope::Root(path)` instances where the path matches.
    /// Workspace-scoped and other instances are untouched.
    async fn shutdown_root_instances(&self, root: &Path) {
        let mut clients = self.clients.lock().await;
        let to_remove: Vec<InstanceKey> = clients
            .keys()
            .filter(|k| matches!(&k.scope, Scope::Root(r) if r.as_path() == root))
            .cloned()
            .collect();
        for key in to_remove {
            if let Some(client_mutex) = clients.remove(&key) {
                info!("Shutting down per-root instance {}", key);
                let mut client = client_mutex.lock().await;
                if client.is_alive()
                    && let Err(e) = client.shutdown().await
                {
                    info!("Failed to shutdown per-root instance {}: {}", key, e);
                }
            }
        }
    }

    /// Spawns legacy per-root instances for newly added roots.
    ///
    /// Only spawns for languages that already have active `Scope::Root`
    /// instances in the map, and only if the new root contains files for
    /// that language (consistent with `spawn_all` behavior).
    async fn spawn_legacy_for_added_roots(&self, added_roots: &[&PathBuf]) {
        // Find languages with active legacy (Scope::Root) instances.
        let clients = self.clients.lock().await.clone();
        let mut legacy_langs: HashMap<String, String> = HashMap::new();
        for key in clients.keys() {
            if matches!(&key.scope, Scope::Root(_)) {
                legacy_langs
                    .entry(key.language_id.clone())
                    .or_insert_with(|| key.server.clone());
            }
        }
        drop(clients);

        if legacy_langs.is_empty() {
            return;
        }

        // Detect which languages have files in the added roots.
        let configured_keys: HashSet<&str> = legacy_langs.keys().map(String::as_str).collect();
        let added_as_owned: Vec<PathBuf> = added_roots.iter().map(|r| (*r).clone()).collect();
        let detected = self
            .fs
            .detect_workspace_languages(&added_as_owned, &configured_keys);

        for lang in &detected {
            let Some(server_name) = legacy_langs.get(lang) else {
                continue;
            };
            for root in added_roots {
                if let Err(e) = self.spawn(server_name, lang, root).await {
                    warn!(
                        source = "lsp.lifecycle",
                        language = lang.as_str(),
                        "Failed to spawn per-root instance for {lang} at {}: {e}",
                        root.display(),
                    );
                }
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

        let roots = self.fs.roots();
        let clients = self.clients.lock().await.clone();

        let mut notified = 0u32;
        for (key, client_mutex) in &clients {
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
                info!("Failed to send didChangeWatchedFiles to {key}: {e}");
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
        for (key, client_mutex) in clients.drain() {
            let mut client = client_mutex.lock().await;
            if client.is_alive() {
                let result = tokio::time::timeout(Duration::from_secs(5), client.shutdown()).await;
                drop(client);
                match result {
                    Ok(Err(e)) => {
                        info!("Failed to shutdown LSP server instance {}: {}", key, e);
                    }
                    Err(_) => {
                        info!(
                            "LSP server instance {} did not respond to shutdown within 5s, killing",
                            key
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

    fn test_fs_with_roots(roots: &[&str]) -> Arc<FilesystemManager> {
        let fs = Arc::new(FilesystemManager::new());
        fs.set_roots(roots.iter().map(PathBuf::from).collect());
        fs
    }

    fn test_config() -> Config {
        Config {
            language: HashMap::new(),
            server: HashMap::new(),
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
            tools: None,
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
                min_severity: None,
                file_patterns: Vec::new(),
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
            tools: None,
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
                min_severity: None,
                file_patterns: Vec::new(),
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
            tools: None,
        }
    }

    #[tokio::test]
    async fn test_roots_returns_initial_roots() -> Result<()> {
        let manager = LspClientManager::new(
            test_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp/root_a", "/tmp/root_b"]),
        );

        let roots = manager.roots();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/tmp/root_a"));
        assert_eq!(roots[1], PathBuf::from("/tmp/root_b"));
        Ok(())
    }

    #[tokio::test]
    async fn test_roots_empty_initial() -> Result<()> {
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());

        assert!(manager.roots().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_remove_root() -> Result<()> {
        let manager = LspClientManager::new(
            test_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp/root_a", "/tmp/root_b"]),
        );

        assert_eq!(manager.roots().len(), 2);

        manager.remove_root(Path::new("/tmp/root_a")).await?;

        let roots = manager.roots();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], PathBuf::from("/tmp/root_b"));
        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_adds_and_removes() -> Result<()> {
        let manager = LspClientManager::new(
            test_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp/root_a", "/tmp/root_b"]),
        );

        // Sync: remove /tmp/root_a, keep /tmp/root_b, add /tmp/root_c
        manager
            .sync_roots(vec![
                PathBuf::from("/tmp/root_b"),
                PathBuf::from("/tmp/root_c"),
            ])
            .await?;

        let roots = manager.roots();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/tmp/root_b"));
        assert_eq!(roots[1], PathBuf::from("/tmp/root_c"));
        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_no_change() -> Result<()> {
        let manager = LspClientManager::new(
            test_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp/root_a"]),
        );

        manager
            .sync_roots(vec![PathBuf::from("/tmp/root_a")])
            .await?;

        let roots = manager.roots();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], PathBuf::from("/tmp/root_a"));
        Ok(())
    }

    /// Checks whether any client in the map has the given language ID.
    fn has_language(clients: &HashMap<InstanceKey, Arc<Mutex<LspClient>>>, lang: &str) -> bool {
        clients.keys().any(|k| k.language_id == lang)
    }

    #[tokio::test]
    async fn test_sync_roots_legacy_removes_per_root() -> Result<()> {
        // mockls without --workspace-folders does NOT advertise workspace folder support.
        // Removing a root should shut down the Scope::Root instance for that root.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = manager.ensure_server_for_language(MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());
        assert!(
            !client.lock().await.supports_workspace_folders(),
            "mockls (no flags) should NOT support workspace folders"
        );

        assert!(has_language(&manager.clients().await, MOCK_LANG_A));

        // sync_roots removes /tmp — the per-root instance should be shut down.
        manager.sync_roots(vec![PathBuf::from("/var")]).await?;

        assert!(
            !has_language(&manager.clients().await, MOCK_LANG_A),
            "Scope::Root(/tmp) instance should be removed when /tmp is dropped"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_legacy_keeps_unchanged_root() -> Result<()> {
        // Adding a root should NOT shut down the existing legacy instance
        // for a root that is still present.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = manager.ensure_server_for_language(MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());

        // sync_roots adds /var but keeps /tmp — the /tmp instance stays.
        manager
            .sync_roots(vec![PathBuf::from("/tmp"), PathBuf::from("/var")])
            .await?;

        assert!(
            has_language(&manager.clients().await, MOCK_LANG_A),
            "Scope::Root(/tmp) instance should remain when /tmp is still a root"
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
                min_severity: None,
                file_patterns: Vec::new(),
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
            tools: None,
        };

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // get_client spawns + initializes; mockls sends workspace/configuration
        // during init. If Catenary responds correctly, initialization succeeds.
        let client = manager.ensure_server_for_language(MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_notifies_supported_client() -> Result<()> {
        // mockls with --workspace-folders DOES advertise workspace folder support.
        // When roots change, it should receive a notification instead of being shut down.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = manager.ensure_server_for_language(MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());
        assert!(
            client.lock().await.supports_workspace_folders(),
            "mockls --workspace-folders should support workspace folders"
        );

        assert!(has_language(&manager.clients().await, MOCK_LANG_A));

        // sync_roots should send notification, NOT shut down the client
        manager
            .sync_roots(vec![PathBuf::from("/tmp"), PathBuf::from("/var")])
            .await?;

        // Client should still be active (not removed)
        assert!(
            has_language(&manager.clients().await, MOCK_LANG_A),
            "mockls client should still be active after sync_roots (workspace folders supported)"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_for_paths_spawns_new_language() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        assert!(manager.clients().await.is_empty());

        // A file with the mock language extension triggers a spawn
        let paths = vec![PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"))];
        manager.ensure_clients_for_paths(&paths).await;

        assert!(
            has_language(&manager.clients().await, MOCK_LANG_A),
            "ensure_clients_for_paths should spawn the mock language server"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_for_paths_skips_existing() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        // Pre-spawn the server
        let _ = manager.ensure_server_for_language(MOCK_LANG_A).await?;
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
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
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
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
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
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        // A file with an unknown extension and no config key should error
        let result = manager.get_client(Path::new("/tmp/test.xyz")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_ensure_document_open_sends_did_open() -> Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = test_fs_with_roots(&[]);
        fs.set_roots(vec![dir.path().to_path_buf()]);

        let manager = LspClientManager::new(mockls_config(), test_logging(), fs);

        let path = dir.path().join(format!("test.{MOCK_LANG_A}"));
        std::fs::write(&path, "content").expect("write");

        let (uri, client_mutex) = manager.ensure_document_open(&path, None).await?;
        assert!(uri.starts_with("file://"));
        assert!(client_mutex.lock().await.is_alive());
        Ok(())
    }

    // --- Scope-aware get_client tests ---

    #[tokio::test]
    async fn test_get_client_workspace_scope() -> Result<()> {
        // File in root resolves to Scope::Workspace client when server
        // supports workspace folders.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        let client = manager.get_client(&path).await?;
        let key = client
            .lock()
            .await
            .server()
            .key()
            .expect("key should be set");
        assert_eq!(key.scope, Scope::Workspace);
        Ok(())
    }

    #[tokio::test]
    async fn test_get_client_root_scope() -> Result<()> {
        // File in root resolves to Scope::Root(root) client when server
        // is legacy (no workspace folder support).
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        let client = manager.get_client(&path).await?;
        let key = client
            .lock()
            .await
            .server()
            .key()
            .expect("key should be set");
        assert_eq!(key.scope, Scope::Root(PathBuf::from("/tmp")));
        Ok(())
    }

    #[tokio::test]
    async fn test_get_client_multi_root_legacy() -> Result<()> {
        // File in root A resolves to Scope::Root(A) instance,
        // file in root B resolves to Scope::Root(B) instance.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        let path_a = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        let client_a = manager.get_client(&path_a).await?;
        let key_a = client_a
            .lock()
            .await
            .server()
            .key()
            .expect("key should be set");
        assert_eq!(key_a.scope, Scope::Root(PathBuf::from("/tmp")));

        let path_b = PathBuf::from(format!("/var/test.{MOCK_LANG_A}"));
        let client_b = manager.get_client(&path_b).await?;
        let key_b = client_b
            .lock()
            .await
            .server()
            .key()
            .expect("key should be set");
        assert_eq!(key_b.scope, Scope::Root(PathBuf::from("/var")));

        // Different Arc instances for different roots.
        assert!(!Arc::ptr_eq(&client_a, &client_b));
        assert_eq!(manager.clients().await.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_get_client_unrooted_errors() {
        // File outside all roots returns error.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let err_msg = manager
            .get_client(Path::new(&format!("/other/test.{MOCK_LANG_A}")))
            .await
            .err()
            .expect("should error for unrooted file")
            .to_string();
        assert!(
            err_msg.contains("outside all workspace roots"),
            "Error should mention workspace roots, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_client_spawns_on_miss() -> Result<()> {
        // First call for a language in a root spawns via ensure_server.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        assert!(manager.clients().await.is_empty());

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        let client = manager.get_client(&path).await?;
        assert!(client.lock().await.is_alive());
        assert_eq!(manager.clients().await.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_get_client_extension_fallback() -> Result<()> {
        // Custom language detected via extension-as-config-key still works.
        // The mock language extension IS the config key (MOCK_LANG_A).
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        // FilesystemManager won't know this extension, but the extension
        // fallback in get_client maps it to the config key.
        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        let client = manager.get_client(&path).await?;
        assert!(client.lock().await.is_alive());
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_for_paths_scope_aware() -> Result<()> {
        // Spawns instances per root, not per language.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        assert!(manager.clients().await.is_empty());

        // Paths in two different roots should spawn two instances.
        let paths = vec![
            PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}")),
            PathBuf::from(format!("/var/test.{MOCK_LANG_A}")),
        ];
        manager.ensure_clients_for_paths(&paths).await;

        let clients = manager.clients().await;
        assert_eq!(
            count_scope(&clients, MOCK_LANG_A, "root"),
            2,
            "Should have two root-scoped instances"
        );
        Ok(())
    }

    // --- Two-step spawn and InstanceKey tests ---

    #[tokio::test]
    async fn test_spawn_workspace_scope() -> Result<()> {
        // mockls with --workspace-folders gets Scope::Workspace key after init.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = manager.ensure_server_for_language(MOCK_LANG_A).await?;
        let key = client
            .lock()
            .await
            .server()
            .key()
            .expect("key should be set after init");
        assert_eq!(key.language_id, MOCK_LANG_A);
        assert_eq!(key.scope, Scope::Workspace);
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_legacy_scope() -> Result<()> {
        // mockls without workspace folders gets Scope::Root(root) key after init.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = manager.ensure_server_for_language(MOCK_LANG_A).await?;
        let key = client
            .lock()
            .await
            .server()
            .key()
            .expect("key should be set after init");
        assert_eq!(key.language_id, MOCK_LANG_A);
        assert_eq!(key.scope, Scope::Root(PathBuf::from("/tmp")));
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_server_idempotent() -> Result<()> {
        // Second call returns same client, no double-spawn.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client1 = manager.ensure_server_for_language(MOCK_LANG_A).await?;
        let client2 = manager.ensure_server_for_language(MOCK_LANG_A).await?;

        // Same Arc — no double spawn
        assert!(Arc::ptr_eq(&client1, &client2));
        assert_eq!(manager.clients().await.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_server_dead_tombstone() -> Result<()> {
        // Dead server returns error on re-ensure.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = manager.ensure_server_for_language(MOCK_LANG_A).await?;
        // Kill the server by shutting it down without removing from map
        client.lock().await.shutdown().await?;
        // Wait briefly for the process to die
        tokio::time::sleep(Duration::from_millis(100)).await;

        let result = manager.ensure_server_for_language(MOCK_LANG_A).await;
        assert!(result.is_err(), "dead server should return error");
        Ok(())
    }

    #[tokio::test]
    async fn test_clients_returns_instance_keys() -> Result<()> {
        // clients() map has InstanceKey keys.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let _ = manager.ensure_server_for_language(MOCK_LANG_A).await?;
        let clients = manager.clients().await;

        assert_eq!(clients.len(), 1);
        let key = clients.keys().next().expect("should have one key");
        assert_eq!(key.language_id, MOCK_LANG_A);
        assert!(
            matches!(key.scope, Scope::Root(_)),
            "mockls without workspace folders should be Root-scoped"
        );
        Ok(())
    }

    // --- Per-root instance lifecycle ---

    /// Helper: count instances with a specific scope kind for a language.
    fn count_scope(
        clients: &HashMap<InstanceKey, Arc<Mutex<LspClient>>>,
        lang: &str,
        scope_kind: &str,
    ) -> usize {
        clients
            .keys()
            .filter(|k| k.language_id == lang && k.scope.kind_str() == scope_kind)
            .count()
    }

    #[tokio::test]
    async fn test_spawn_all_multi_root_legacy() -> Result<()> {
        // Legacy server (no workspace folders) should get one instance per root.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        manager.spawn_all().await;

        // The mock language uses extension-based detection via the fallback path.
        // Neither /tmp nor /var will have files matching the mock extension,
        // so spawn_all detects nothing. Instead, manually spawn to test
        // the multi-root expansion logic.
        let _ = manager.ensure_server_for_language(MOCK_LANG_A).await?;

        // First root spawned. Now check that spawn() can create a second
        // instance for the other root.
        let server_name = format!("mockls-{MOCK_LANG_A}");
        let (_key, _client) = manager
            .spawn(&server_name, MOCK_LANG_A, Path::new("/var"))
            .await?;

        let clients = manager.clients().await;
        assert_eq!(
            count_scope(&clients, MOCK_LANG_A, "root"),
            2,
            "Legacy server should have two root-scoped instances"
        );

        // Verify distinct root paths.
        let root_paths: HashSet<PathBuf> = clients
            .keys()
            .filter(|k| k.language_id == MOCK_LANG_A)
            .filter_map(|k| k.scope.root_path().map(Path::to_path_buf))
            .collect();
        assert!(root_paths.contains(&PathBuf::from("/tmp")));
        assert!(root_paths.contains(&PathBuf::from("/var")));

        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_all_multi_root_workspace() -> Result<()> {
        // Workspace-capable server should be spawned once with Scope::Workspace.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        let _ = manager.ensure_server_for_language(MOCK_LANG_A).await?;

        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            1,
            "Workspace server should have one instance"
        );
        let key = clients.keys().next().expect("should have one key");
        assert_eq!(key.scope, Scope::Workspace);

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_workspace_unchanged() -> Result<()> {
        // Workspace-capable server should NOT be shut down on root change.
        // Regression test for restart hack removal.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = manager.ensure_server_for_language(MOCK_LANG_A).await?;
        assert!(client.lock().await.supports_workspace_folders());

        manager
            .sync_roots(vec![PathBuf::from("/tmp"), PathBuf::from("/var")])
            .await?;

        let clients = manager.clients().await;
        assert!(
            has_language(&clients, MOCK_LANG_A),
            "Workspace server should stay alive after sync_roots"
        );
        let key = clients.keys().next().expect("should have one key");
        assert_eq!(
            key.scope,
            Scope::Workspace,
            "Key should remain Scope::Workspace"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_remove_root_legacy_shutdown() -> Result<()> {
        // remove_root should shut down the Scope::Root instance for the removed root.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = manager.ensure_server_for_language(MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());

        manager.remove_root(Path::new("/tmp")).await?;

        assert!(
            !has_language(&manager.clients().await, MOCK_LANG_A),
            "Per-root instance should be removed after remove_root"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_remove_root_workspace_notified() -> Result<()> {
        // Workspace-capable server stays alive after remove_root.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        let client = manager.ensure_server_for_language(MOCK_LANG_A).await?;
        assert!(client.lock().await.supports_workspace_folders());

        manager.remove_root(Path::new("/tmp")).await?;

        assert!(
            has_language(&manager.clients().await, MOCK_LANG_A),
            "Workspace server should stay alive after remove_root"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_no_change_noop() -> Result<()> {
        // Identical root set produces no spawns or shutdowns.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let _ = manager.ensure_server_for_language(MOCK_LANG_A).await?;
        let before = manager.clients().await.len();

        manager.sync_roots(vec![PathBuf::from("/tmp")]).await?;

        assert_eq!(
            manager.clients().await.len(),
            before,
            "No-change sync should not alter client count"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_shutdown_root_instances_selective() -> Result<()> {
        // Only Scope::Root instances matching the root are shut down.
        // Other roots and workspace instances are untouched.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        // Spawn two root-scoped instances.
        let _ = manager.ensure_server_for_language(MOCK_LANG_A).await?;
        let server_name = format!("mockls-{MOCK_LANG_A}");
        let _ = manager
            .spawn(&server_name, MOCK_LANG_A, Path::new("/var"))
            .await?;

        assert_eq!(manager.clients().await.len(), 2);

        // Shut down only /var instances.
        manager.shutdown_root_instances(Path::new("/var")).await;

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 1, "Only /var instance should be removed");
        let remaining_key = clients.keys().next().expect("one remaining");
        assert_eq!(
            remaining_key.scope,
            Scope::Root(PathBuf::from("/tmp")),
            "/tmp instance should remain"
        );

        Ok(())
    }

    // --- ServerStatus enrichment ---

    #[tokio::test]
    async fn test_server_status_enriched() -> Result<()> {
        // status(&key) populates server_name, scope_kind, scope_root.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = manager.ensure_server_for_language(MOCK_LANG_A).await?;
        let locked = client.lock().await;
        let key = locked.server().key().expect("key should be set");
        let status = locked.status(&key);
        drop(locked);

        assert_eq!(status.language, MOCK_LANG_A);
        assert_eq!(status.server_name, format!("mockls-{MOCK_LANG_A}"));
        assert_eq!(status.scope_kind, "root");
        assert_eq!(status.scope_root, "/tmp");
        assert_eq!(status.state.display_state(), "initializing");
        Ok(())
    }

    #[tokio::test]
    async fn test_server_status_workspace_scope() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = manager.ensure_server_for_language(MOCK_LANG_A).await?;
        let locked = client.lock().await;
        let key = locked.server().key().expect("key should be set");
        let status = locked.status(&key);
        drop(locked);

        assert_eq!(status.scope_kind, "workspace");
        assert!(
            status.scope_root.is_empty(),
            "workspace scope should have empty scope_root"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_all_server_status_multi_instance() -> Result<()> {
        // Two instances of the same language produce two status entries
        // with different scope info.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        let _ = manager.ensure_server_for_language(MOCK_LANG_A).await?;
        let server_name = format!("mockls-{MOCK_LANG_A}");
        let _ = manager
            .spawn(&server_name, MOCK_LANG_A, Path::new("/var"))
            .await?;

        let statuses = manager.all_server_status().await;
        assert_eq!(statuses.len(), 2, "should have two status entries");

        let roots: HashSet<String> = statuses.iter().map(|s| s.scope_root.clone()).collect();
        assert!(roots.contains("/tmp"), "should include /tmp root");
        assert!(roots.contains("/var"), "should include /var root");

        for s in &statuses {
            assert_eq!(s.language, MOCK_LANG_A);
            assert_eq!(s.server_name, server_name);
            assert_eq!(s.scope_kind, "root");
        }

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

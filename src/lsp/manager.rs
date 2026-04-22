// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

use anyhow::{Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::bridge::filesystem_manager::FilesystemManager;
use crate::config::{Config, ServerBinding, ServerDef};
use crate::logging::LoggingServer;
use crate::lsp::LspClient;
use crate::lsp::glob::{FileChange, GlobPattern, LspGlob, WatchKind};
use crate::lsp::instance_key::{InstanceKey, Scope};
use crate::lsp::server::LspServer;
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

/// Looks up an existing client instance by trying `Scope::Root(root)`
/// first, then `Scope::Workspace`. Root-first ordering ensures that
/// project-scoped instances (Rule A) take precedence over shared
/// workspace instances when both exist for the same `(lang, server)`.
fn find_instance(
    clients: &HashMap<InstanceKey, Arc<Mutex<LspClient>>>,
    lang: &str,
    server_name: &str,
    root: &Path,
) -> Option<Arc<Mutex<LspClient>>> {
    let root_key = InstanceKey::new(
        lang.to_string(),
        server_name.to_string(),
        Scope::Root(root.to_path_buf()),
    );
    if let Some(client) = clients.get(&root_key) {
        return Some(client.clone());
    }
    let workspace_key =
        InstanceKey::new(lang.to_string(), server_name.to_string(), Scope::Workspace);
    clients.get(&workspace_key).cloned()
}

/// Tests whether a path matches a server's `file_patterns`.
///
/// If `patterns` is empty, returns `true` (no filter = match all).
/// Otherwise, matches the filename component of `path` against the
/// compiled globs.
fn file_matches_patterns(path: &Path, patterns: &[LspGlob]) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let Some(file_name) = path.file_name() else {
        return false;
    };
    let file_path = Path::new(file_name);
    patterns.iter().any(|g| g.is_match(file_path))
}

/// Manages the lifecycle of LSP clients, document state, and language detection.
///
/// Single authority for LSP server spawning, caching, shutdown, and document
/// lifecycle. Document versioning and open/close tracking live on each
/// [`LspClient`] — each server sees an independent monotonic version sequence.
pub struct LspClientManager {
    config: Config,
    /// Per-root project configs from `.catenary.toml`. Keyed by root path.
    /// Uses `std::sync::Mutex` — reads are fast, non-contended, and must
    /// not be held across `.await` points.
    project_configs: std::sync::Mutex<HashMap<PathBuf, crate::config::ProjectConfig>>,
    clients: Mutex<HashMap<InstanceKey, Arc<Mutex<LspClient>>>>,
    logging: LoggingServer,
    fs: Arc<FilesystemManager>,
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
            project_configs: std::sync::Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            logging,
            fs,
        }
    }

    /// Returns a reference to the configuration.
    pub const fn config(&self) -> &Config {
        &self.config
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

        // Load project configs for all roots.
        self.load_project_configs_for_roots(&roots);

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
            let Some(lang_config) = self.config.resolve_language(lang) else {
                continue;
            };
            let bindings: Vec<ServerBinding> = lang_config.servers.clone();

            for binding in &bindings {
                let Some(first_root) = roots.first() else {
                    continue;
                };

                let client = match self.ensure_server(lang, &binding.name, first_root).await {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(
                            source = "lsp.lifecycle",
                            language = lang.as_str(),
                            server = binding.name.as_str(),
                            "Failed to spawn LSP server for {lang}: {e}",
                        );
                        continue;
                    }
                };

                if roots.len() <= 1 {
                    continue;
                }

                let key = client.lock().await.server().key();
                let Some(key) = key else { continue };

                // Workspace-capable servers already received all roots in the
                // `initialize` request — no additional notification needed.
                // Legacy servers need a separate instance per remaining root.
                if let Scope::Root(_) = key.scope {
                    info!(
                        source = "lsp.lifecycle",
                        language = lang.as_str(),
                        server = binding.name.as_str(),
                        "Server does not support workspaceFolders — spawning per-root instances",
                    );
                    for root in &roots[1..] {
                        if let Err(e) = self.spawn(&binding.name, lang, root).await {
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

        // Remove project config for the removed root.
        self.project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(root);

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

        // Update project configs: load for added roots, remove for removed.
        self.load_project_configs_for_roots(&to_add);
        if !to_remove.is_empty() {
            let mut configs = self
                .project_configs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for removed in &to_remove {
                configs.remove(removed);
            }
        }

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

    /// Returns clients for a file path, filtered by capability and
    /// `file_patterns`, in priority order (from the `servers` list in
    /// `[language.*]`).
    ///
    /// Resolves language from path via `FilesystemManager`, iterates
    /// the binding's servers, filters by:
    /// 1. `file_patterns` on `[server.*]` (filename-level glob)
    /// 2. The given capability check
    ///
    /// Returns an empty Vec when no server matches. On empty result,
    /// emits a `warn!()` — dedup handled by `NotificationQueueSink`.
    ///
    /// Does not block on server readiness — callers must call
    /// `wait_ready_for_path` or `wait_ready_all` before invoking.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "clients lock held across async iteration for consistent snapshot"
    )]
    pub async fn get_servers(
        &self,
        path: &Path,
        capability: fn(&LspServer) -> bool,
    ) -> Vec<Arc<Mutex<LspClient>>> {
        // Detect language: primary (FilesystemManager) then fallback (raw extension).
        let Some(lang_id) = self.fs.language_id(path).or_else(|| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(str::to_string)
        }) else {
            return Vec::new();
        };

        // Resolve owning workspace root.
        let Some(root) = self.fs.resolve_root(path) else {
            return Vec::new();
        };

        // Look up language config.
        let Some(lang_config) = self.config.resolve_language(&lang_id) else {
            return Vec::new();
        };

        let clients = self.clients.lock().await;
        let mut result = Vec::new();

        for binding in &lang_config.servers {
            // Look up the ServerDef for file_patterns.
            let Some(server_def) = self.config.server.get(&binding.name) else {
                continue;
            };

            // file_patterns filter: if non-empty, filename must match.
            if !file_matches_patterns(path, &server_def.compiled_patterns) {
                continue;
            }

            // Instance lookup: tries Workspace then Root(root).
            let Some(client) = find_instance(&clients, &lang_id, &binding.name, &root) else {
                continue;
            };

            // Liveness check.
            let locked = client.lock().await;
            if !locked.is_alive() {
                continue;
            }

            // Capability check (conservatively false for uninitialized servers).
            if !capability(locked.server()) {
                continue;
            }
            drop(locked);

            result.push(client);
        }

        if result.is_empty() && !lang_config.servers.is_empty() {
            warn!(
                source = "lsp.routing",
                language = lang_id.as_str(),
                "No server supports the requested capability for {lang_id} files",
            );
        }

        result
    }

    /// Waits for every server bound to this path's language binding.
    ///
    /// Resolves language from path, iterates all servers in the
    /// binding, waits for each to reach Ready or terminal state.
    /// Dead servers don't block — they return immediately. Servers
    /// that haven't been spawned yet are skipped (not spawned).
    pub async fn wait_ready_for_path(&self, path: &Path) {
        // Detect language: primary (FilesystemManager) then fallback (raw extension).
        let Some(lang_id) = self.fs.language_id(path).or_else(|| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(str::to_string)
        }) else {
            return;
        };

        // Resolve owning workspace root — unrooted files skip waiting.
        let Some(root) = self.fs.resolve_root(path) else {
            return;
        };

        // Look up language config — unconfigured languages skip.
        let Some(lang_config) = self.config.resolve_language(&lang_id) else {
            return;
        };

        // Collect matching instances under the lock, then release before waiting.
        let to_wait: Vec<Arc<Mutex<LspClient>>> = {
            let clients = self.clients.lock().await;
            lang_config
                .servers
                .iter()
                .filter_map(|binding| find_instance(&clients, &lang_id, &binding.name, &root))
                .collect()
        };

        for client_mutex in to_wait {
            client_mutex.lock().await.wait_ready().await;
        }
    }

    /// Waits for every active instance across all bindings.
    ///
    /// Clones the client map, waits for each to reach Ready or
    /// terminal state. Dead servers return immediately.
    pub async fn wait_ready_all(&self) {
        let clients = self.clients.lock().await.clone();
        for (_key, client_mutex) in clients {
            client_mutex.lock().await.wait_ready().await;
        }
    }

    /// Spawns missing servers for the given paths and waits for
    /// all to be ready.
    ///
    /// Combines [`ensure_clients_for_paths`](Self::ensure_clients_for_paths)
    /// (spawn) with [`wait_ready_all`](Self::wait_ready_all). Closes the
    /// lazy-spawn gap: after this call, all servers for the discovered
    /// languages are Ready (or terminal).
    pub async fn ensure_and_wait_for_paths(&self, paths: &[PathBuf]) {
        self.ensure_clients_for_paths(paths).await;
        self.wait_ready_all().await;
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

        // Build per-root settings map from project configs.
        // Workspace instances use this for scopeUri resolution;
        // root instances won't have scopeUri requests for other roots.
        let settings_per_root = self.collect_per_root_settings(server_name, &roots);

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
            settings_per_root,
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

    /// Opens a document on a specific client.
    ///
    /// Reads the file, checks per-client open state, sends `didOpen` or
    /// `didChange` as appropriate. Version tracking is per-client — each
    /// server gets an independent monotonic sequence starting at 1.
    ///
    /// Used by request/response dispatch: the caller gets clients from
    /// [`get_servers`](Self::get_servers) and opens the document on each
    /// as it iterates the priority chain.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or the LSP notification
    /// fails.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "Client lock held across notification send"
    )]
    pub async fn open_document_on(
        &self,
        path: &Path,
        client: &Arc<Mutex<LspClient>>,
        parent_id: Option<i64>,
    ) -> Result<String> {
        let canonical = path.canonicalize()?;
        let uri = crate::lsp::lang::path_to_uri(&canonical);
        let text = tokio::fs::read_to_string(&canonical).await?;

        let mut client = client.lock().await;
        client.set_parent_id(parent_id);

        if !client.is_alive() {
            client.set_parent_id(None);
            return Err(anyhow!(
                "[{}] server is no longer running",
                client.language()
            ));
        }

        let (first_open, version) = client.open_document(&uri);
        if first_open {
            let language_id = self
                .fs
                .language_id(path)
                .unwrap_or_else(|| "plaintext".to_string());
            client.did_open(&uri, &language_id, version, &text).await?;
        } else {
            client.did_change(&uri, version, &text).await?;
        }

        drop(client);
        Ok(uri)
    }

    /// Returns diagnostic-enabled servers for a file path without opening
    /// the document.
    ///
    /// Applies both the capability gate ([`LspServer::supports_diagnostics`])
    /// and the config-level filter ([`LanguageConfig::diagnostics_enabled`]).
    /// Returns an empty Vec when no server qualifies.
    pub async fn diagnostic_servers(&self, path: &Path) -> Vec<Arc<Mutex<LspClient>>> {
        let servers = self
            .get_servers(path, LspServer::supports_diagnostics)
            .await;

        if servers.is_empty() {
            return Vec::new();
        }

        let lang_id = self.fs.language_id(path).or_else(|| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(str::to_string)
        });
        let lang_config = lang_id
            .as_deref()
            .and_then(|id| self.config.resolve_language(id));

        let mut clients = Vec::new();
        for client in &servers {
            let server_name = client.lock().await.server_name().to_string();
            let enabled = lang_config
                .as_ref()
                .is_some_and(|lc| lc.diagnostics_enabled(&server_name));
            if enabled {
                clients.push(client.clone());
            }
        }

        clients
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

                // Check all servers in the binding, not just the first.
                for binding in &lang_config.servers {
                    if find_instance(&active, &lang, &binding.name, &root).is_none() {
                        to_spawn.insert((lang.clone(), binding.name.clone(), root.clone()));
                    }
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
        // Multiple servers may exist per language — collect all unique names.
        let clients = self.clients.lock().await.clone();
        let mut legacy_langs: HashMap<String, Vec<String>> = HashMap::new();
        for key in clients.keys() {
            if matches!(&key.scope, Scope::Root(_)) {
                let servers = legacy_langs.entry(key.language_id.clone()).or_default();
                if !servers.contains(&key.server) {
                    servers.push(key.server.clone());
                }
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
            let Some(servers) = legacy_langs.get(lang) else {
                continue;
            };
            for server_name in servers {
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

    /// Whether a language is project-scoped in the given root.
    ///
    /// Rule A: returns `true` if the root's project config has a
    /// `[language.{lang}]` entry. This triggers tier 1 — an
    /// isolated per-root instance.
    #[must_use]
    pub fn is_project_scoped(&self, lang: &str, root: &Path) -> bool {
        let configs = self
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        configs
            .get(root)
            .is_some_and(|pc| pc.language.contains_key(lang))
    }

    /// Returns the effective `ServerDef` for a server in a root.
    ///
    /// Deep-merges the root's project `[server.{name}]` (if any)
    /// over the user-level `[server.{name}]`. Returns user-level
    /// def unchanged if no project override exists.
    #[must_use]
    pub fn effective_server_def(&self, server_name: &str, root: &Path) -> Option<ServerDef> {
        let user_def = self.config.server.get(server_name)?;

        let project_def = self
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(root)
            .and_then(|pc| pc.server.get(server_name))
            .cloned();

        let Some(project_def) = project_def else {
            return Some(user_def.clone());
        };

        // Field-level merge: project fields override user fields when
        // present. Settings use deep_merge for nested object merging.
        let mut merged = user_def.clone();
        if !project_def.command.is_empty() {
            merged.command.clone_from(&project_def.command);
            merged.args.clone_from(&project_def.args);
        }
        if project_def.initialization_options.is_some() {
            merged
                .initialization_options
                .clone_from(&project_def.initialization_options);
        }
        if project_def.min_severity.is_some() {
            merged.min_severity.clone_from(&project_def.min_severity);
        }
        if !project_def.file_patterns.is_empty() {
            merged.file_patterns.clone_from(&project_def.file_patterns);
            merged
                .compiled_patterns
                .clone_from(&project_def.compiled_patterns);
        }
        if let Some(ref project_settings) = project_def.settings {
            if let Some(ref user_settings) = user_def.settings {
                merged.settings = Some(crate::config::merge::deep_merge(
                    user_settings,
                    project_settings,
                ));
            } else {
                merged.settings = Some(project_settings.clone());
            }
        }

        Some(merged)
    }

    /// Returns the effective settings `Value` for a server in a root.
    ///
    /// Deep-merges the root's project `[server.{name}].settings`
    /// over the user-level `[server.{name}].settings`.
    #[must_use]
    pub fn effective_settings(&self, server_name: &str, root: &Path) -> Option<serde_json::Value> {
        let user_settings = self
            .config
            .server
            .get(server_name)
            .and_then(|d| d.settings.clone());

        let project_settings = self
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(root)
            .and_then(|pc| pc.server.get(server_name))
            .and_then(|d| d.settings.clone());

        match (user_settings, project_settings) {
            (Some(user), Some(project)) => Some(crate::config::merge::deep_merge(&user, &project)),
            (None, Some(project)) => Some(project),
            (Some(user), None) => Some(user),
            (None, None) => None,
        }
    }

    /// Collects per-root settings overrides for a server.
    ///
    /// Returns a map from root path to the project-config settings overlay
    /// for roots whose `.catenary.toml` has `[server.{name}.settings]`.
    /// Empty entries (roots without project config or without settings for
    /// this server) are omitted.
    fn collect_per_root_settings(
        &self,
        server_name: &str,
        roots: &[PathBuf],
    ) -> HashMap<PathBuf, serde_json::Value> {
        let configs = self
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut map = HashMap::new();
        for root in roots {
            if let Some(settings) = configs
                .get(root)
                .and_then(|pc| pc.server.get(server_name))
                .and_then(|d| d.settings.clone())
            {
                map.insert(root.clone(), settings);
            }
        }
        map
    }

    /// Loads project configs for the given roots.
    ///
    /// For each root, discovers `.catenary.toml` via [`crate::config::load_project_config`]
    /// and stores the result. Errors are logged and skipped — a broken
    /// project config should not prevent other roots from loading.
    fn load_project_configs_for_roots(&self, roots: &[PathBuf]) {
        for root in roots {
            match crate::config::load_project_config(root) {
                Ok(Some(pc)) => {
                    info!(
                        source = "config.project",
                        root = %root.display(),
                        "Loaded project config from {}",
                        root.join(".catenary.toml").display(),
                    );
                    crate::config::validate::warn_orphan_project_servers(&pc, &self.config, root);
                    self.project_configs
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(root.clone(), pc);
                }
                Ok(None) => {} // No project config — fine.
                Err(e) => {
                    warn!(
                        source = "config.project",
                        root = %root.display(),
                        "Failed to load project config from {}: {e}",
                        root.join(".catenary.toml").display(),
                    );
                }
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
            resolved_commands: None,
        }
    }

    /// Test helper: spawns the first server for a language using the first root.
    ///
    /// Replaces the removed `ensure_server_for_language` for test convenience.
    async fn ensure_first_server(
        manager: &LspClientManager,
        lang: &str,
    ) -> Result<Arc<Mutex<LspClient>>> {
        let lang_config = manager
            .config
            .resolve_language(lang)
            .ok_or_else(|| anyhow!("No LSP server configured for language '{lang}'"))?;
        let server_name = &lang_config
            .servers
            .first()
            .ok_or_else(|| anyhow!("No servers configured for language '{lang}'"))?
            .name;
        let root = manager
            .fs
            .roots()
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("No workspace roots available for spawning '{lang}'"))?;
        manager.ensure_server(lang, server_name, &root).await
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
                compiled_patterns: Vec::new(),
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
            resolved_commands: None,
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
                compiled_patterns: Vec::new(),
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
            resolved_commands: None,
        }
    }

    /// Config with two legacy mockls servers for the same language.
    fn mockls_multi_server_config() -> Config {
        let bin = mockls_bin();
        let server_a = format!("mockls-{MOCK_LANG_A}-a");
        let server_b = format!("mockls-{MOCK_LANG_A}-b");
        let mut server = HashMap::new();
        for name in [&server_a, &server_b] {
            server.insert(
                name.clone(),
                ServerDef {
                    command: bin.to_string_lossy().to_string(),
                    args: vec![MOCK_LANG_A.to_string()],
                    initialization_options: None,
                    settings: None,
                    min_severity: None,
                    file_patterns: Vec::new(),
                    compiled_patterns: Vec::new(),
                },
            );
        }
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_a), ServerBinding::new(server_b)],
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
            resolved_commands: None,
        }
    }

    /// Config with two workspace-folders-capable mockls servers for the same language.
    fn mockls_multi_server_workspace_config() -> Config {
        let bin = mockls_bin();
        let server_a = format!("mockls-{MOCK_LANG_A}-wf-a");
        let server_b = format!("mockls-{MOCK_LANG_A}-wf-b");
        let mut server = HashMap::new();
        for name in [&server_a, &server_b] {
            server.insert(
                name.clone(),
                ServerDef {
                    command: bin.to_string_lossy().to_string(),
                    args: vec![MOCK_LANG_A.to_string(), "--workspace-folders".to_string()],
                    initialization_options: None,
                    settings: None,
                    min_severity: None,
                    file_patterns: Vec::new(),
                    compiled_patterns: Vec::new(),
                },
            );
        }
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_a), ServerBinding::new(server_b)],
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
            resolved_commands: None,
        }
    }

    /// Config with one workspace-capable and one legacy mockls for the same language.
    fn mockls_mixed_capability_config() -> Config {
        let bin = mockls_bin();
        let server_ws = format!("mockls-{MOCK_LANG_A}-ws");
        let server_legacy = format!("mockls-{MOCK_LANG_A}-leg");
        let mut server = HashMap::new();
        server.insert(
            server_ws.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![MOCK_LANG_A.to_string(), "--workspace-folders".to_string()],
                initialization_options: None,
                settings: None,
                min_severity: None,
                file_patterns: Vec::new(),
                compiled_patterns: Vec::new(),
            },
        );
        server.insert(
            server_legacy.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![MOCK_LANG_A.to_string()],
                initialization_options: None,
                settings: None,
                min_severity: None,
                file_patterns: Vec::new(),
                compiled_patterns: Vec::new(),
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![
                    ServerBinding::new(server_ws),
                    ServerBinding::new(server_legacy),
                ],
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
            resolved_commands: None,
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

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
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

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
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
                compiled_patterns: Vec::new(),
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
            resolved_commands: None,
        };

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // ensure_first_server spawns + initializes; mockls sends workspace/configuration
        // during init. If Catenary responds correctly, initialization succeeds.
        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
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

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
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
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
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

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
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

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
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

        let client1 = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let client2 = ensure_first_server(&manager, MOCK_LANG_A).await?;

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

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        // Kill the server by shutting it down without removing from map
        client.lock().await.shutdown().await?;
        // Wait briefly for the process to die
        tokio::time::sleep(Duration::from_millis(100)).await;

        let result = ensure_first_server(&manager, MOCK_LANG_A).await;
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

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
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
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

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

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

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

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
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

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
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

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
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

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
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
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
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

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
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

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
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

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
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

    // --- file_matches_patterns ---

    mod file_patterns_matching {
        use super::*;
        use crate::lsp::glob::LspGlob;

        fn compile(patterns: &[&str]) -> Vec<LspGlob> {
            patterns
                .iter()
                .map(|p| LspGlob::new(p).expect("valid glob"))
                .collect()
        }

        #[test]
        fn empty_patterns_matches_all() {
            assert!(file_matches_patterns(Path::new("/tmp/test.rs"), &[]));
            assert!(file_matches_patterns(Path::new("/tmp/PKGBUILD"), &[]));
        }

        #[test]
        fn exact_filename_match() {
            let patterns = compile(&["PKGBUILD"]);
            assert!(file_matches_patterns(
                Path::new("/home/user/PKGBUILD"),
                &patterns
            ));
        }

        #[test]
        fn exact_filename_no_match() {
            let patterns = compile(&["PKGBUILD"]);
            assert!(!file_matches_patterns(
                Path::new("/home/user/script.sh"),
                &patterns
            ));
        }

        #[test]
        fn glob_extension_match() {
            let patterns = compile(&["*.ebuild"]);
            assert!(file_matches_patterns(
                Path::new("/repo/foo.ebuild"),
                &patterns
            ));
        }

        #[test]
        fn glob_extension_no_match() {
            let patterns = compile(&["*.ebuild"]);
            assert!(!file_matches_patterns(Path::new("/repo/foo.rs"), &patterns));
        }

        #[test]
        fn multiple_patterns_any_match() {
            let patterns = compile(&["PKGBUILD", "*.ebuild"]);
            assert!(file_matches_patterns(
                Path::new("/repo/PKGBUILD"),
                &patterns
            ));
            assert!(file_matches_patterns(
                Path::new("/repo/foo.ebuild"),
                &patterns
            ));
            assert!(!file_matches_patterns(
                Path::new("/repo/script.sh"),
                &patterns
            ));
        }

        #[test]
        fn no_filename_returns_false() {
            // A path that is just "/" has no file_name component.
            let patterns = compile(&["*"]);
            assert!(!file_matches_patterns(Path::new("/"), &patterns));
        }

        #[test]
        fn star_does_not_cross_separator() {
            // LspGlob uses literal_separator(true): * should not match paths.
            let patterns = compile(&["*.rs"]);
            // "foo.rs" matches
            assert!(file_matches_patterns(Path::new("/tmp/foo.rs"), &patterns));
            // "src/foo.rs" as a single filename component would not occur,
            // but matching against just the filename means this works normally.
            assert!(file_matches_patterns(
                Path::new("/project/src/foo.rs"),
                &patterns
            ));
        }
    }

    // --- get_servers ---

    #[tokio::test]
    async fn test_get_servers_single_server() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        // Pre-spawn the server
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        // Use a capability that mockls supports (document symbols — all mockls
        // instances advertise it).
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols)
            .await;
        assert_eq!(servers.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_capability_filter() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        // Use a capability that mockls does NOT support (pull diagnostics
        // requires --pull-diagnostics flag which mockls_config doesn't set).
        let servers = manager
            .get_servers(&path, LspServer::supports_pull_diagnostics)
            .await;
        assert!(
            servers.is_empty(),
            "mockls (default) does not support pull diagnostics, should return empty"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_file_patterns_match() -> Result<()> {
        // file_patterns filters within the language. Use a pattern that
        // matches the filename of a file with the mock extension.
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-fp");
        let pattern = "special.*".to_string();
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![MOCK_LANG_A.to_string()],
                initialization_options: None,
                settings: None,
                min_severity: None,
                file_patterns: vec![pattern.clone()],
                compiled_patterns: vec![
                    crate::lsp::glob::LspGlob::new(&pattern).expect("valid glob"),
                ],
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
            resolved_commands: None,
        };

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        // Filename "special.yX4Za" matches pattern "special.*"
        let path = PathBuf::from(format!("/tmp/special.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols)
            .await;
        assert_eq!(
            servers.len(),
            1,
            "special.{MOCK_LANG_A} should match file_patterns=[\"special.*\"]"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_file_patterns_no_match() -> Result<()> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-fp2");
        let pattern = "special.*".to_string();
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![MOCK_LANG_A.to_string()],
                initialization_options: None,
                settings: None,
                min_severity: None,
                file_patterns: vec![pattern.clone()],
                compiled_patterns: vec![
                    crate::lsp::glob::LspGlob::new(&pattern).expect("valid glob"),
                ],
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
            resolved_commands: None,
        };

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        // Filename "other.yX4Za" does NOT match pattern "special.*"
        let path = PathBuf::from(format!("/tmp/other.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols)
            .await;
        assert!(
            servers.is_empty(),
            "other.{MOCK_LANG_A} should not match file_patterns=[\"special.*\"]"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_file_patterns_glob() -> Result<()> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-fpg");
        let pattern = format!("*.{MOCK_LANG_A}");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![MOCK_LANG_A.to_string()],
                initialization_options: None,
                settings: None,
                min_severity: None,
                file_patterns: vec![pattern.clone()],
                compiled_patterns: vec![
                    crate::lsp::glob::LspGlob::new(&pattern).expect("valid glob"),
                ],
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
            resolved_commands: None,
        };

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/foo.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols)
            .await;
        assert_eq!(servers.len(), 1, "*.ext glob should match");
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_empty_file_patterns() -> Result<()> {
        // Server with no file_patterns matches all files for the language.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/anything.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols)
            .await;
        assert_eq!(
            servers.len(),
            1,
            "empty file_patterns should match all files"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_dead_server_skipped() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        // Kill the server
        client.lock().await.shutdown().await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols)
            .await;
        assert!(servers.is_empty(), "dead server should be skipped");
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_outside_roots() {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let path = PathBuf::from(format!("/other/test.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols)
            .await;
        assert!(servers.is_empty(), "file outside roots should return empty");
    }

    #[tokio::test]
    async fn test_get_servers_unknown_language() {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let servers = manager
            .get_servers(Path::new("/tmp/test.xyz"), LspServer::supports_hover)
            .await;
        assert!(servers.is_empty(), "unknown language should return empty");
    }

    #[tokio::test]
    async fn test_get_servers_priority_order() -> Result<()> {
        // With multiple servers in the binding, result preserves order.
        // (Currently only one server per language is spawned, so this test
        // exercises the path ordering with a single entry — 1c-01 will
        // extend it to multiple.)
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols)
            .await;
        assert_eq!(servers.len(), 1);
        Ok(())
    }

    // --- Multi-server spawning (1c-01) ---

    #[tokio::test]
    async fn test_spawn_all_multi_server() -> Result<()> {
        // Two workspace-capable servers for one language: spawn_all creates
        // two entries in the client map with different InstanceKeys.
        let config = mockls_multi_server_workspace_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers
            .iter()
            .map(|b| b.name.clone())
            .collect();
        assert_eq!(bindings.len(), 2);

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // spawn_all won't detect files (synthetic extension), so spawn directly
        // using the same pattern spawn_all uses.
        for name in &bindings {
            manager
                .ensure_server(MOCK_LANG_A, name, Path::new("/tmp"))
                .await?;
        }

        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            2,
            "Two servers should produce two client map entries"
        );

        let server_names: HashSet<String> = clients.keys().map(|k| k.server.clone()).collect();
        assert!(server_names.contains(&bindings[0]));
        assert!(server_names.contains(&bindings[1]));
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_all_multi_server_legacy() -> Result<()> {
        // Two legacy servers, two roots: 2 servers × 2 roots = 4 instances.
        let config = mockls_multi_server_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers
            .iter()
            .map(|b| b.name.clone())
            .collect();

        let manager = LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        // Simulate spawn_all's multi-server + per-root logic.
        let roots = manager.roots();
        for name in &bindings {
            let client = manager.ensure_server(MOCK_LANG_A, name, &roots[0]).await?;
            let key = client.lock().await.server().key();
            let Some(key) = key else {
                continue;
            };
            if matches!(key.scope, Scope::Root(_)) {
                for root in &roots[1..] {
                    manager.spawn(name, MOCK_LANG_A, root).await?;
                }
            }
        }

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 4, "2 legacy servers × 2 roots = 4 instances");
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 4);
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_all_mixed_capability() -> Result<()> {
        // One workspace-capable + one legacy server, two roots:
        // workspace gets 1 instance, legacy gets 2.
        let config = mockls_mixed_capability_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers
            .iter()
            .map(|b| b.name.clone())
            .collect();

        let manager = LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        let roots = manager.roots();
        for name in &bindings {
            let client = manager.ensure_server(MOCK_LANG_A, name, &roots[0]).await?;
            let key = client.lock().await.server().key();
            let Some(key) = key else {
                continue;
            };
            if matches!(key.scope, Scope::Root(_)) {
                for root in &roots[1..] {
                    manager.spawn(name, MOCK_LANG_A, root).await?;
                }
            }
        }

        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            3,
            "1 workspace + 2 legacy per-root = 3 instances"
        );
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "workspace"), 1);
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_for_paths_multi_server() -> Result<()> {
        // New files trigger spawning of all servers in the binding.
        let manager = LspClientManager::new(
            mockls_multi_server_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        assert!(manager.clients().await.is_empty());

        let paths = vec![PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"))];
        manager.ensure_clients_for_paths(&paths).await;

        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            2,
            "ensure_clients_for_paths should spawn all servers in the binding"
        );

        let server_names: HashSet<String> = clients.keys().map(|k| k.server.clone()).collect();
        assert_eq!(server_names.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_legacy_for_added_roots_multi_server() -> Result<()> {
        // Adding a root spawns per-root instances for all legacy servers.
        // Uses a tempdir with real files so detect_workspace_languages succeeds.
        let root_a = tempfile::tempdir().expect("tempdir");
        let root_b = tempfile::tempdir().expect("tempdir");

        // Create files with the synthetic extension so language detection works.
        std::fs::write(root_a.path().join(format!("file.{MOCK_LANG_A}")), "content")
            .expect("write");
        std::fs::write(root_b.path().join(format!("file.{MOCK_LANG_A}")), "content")
            .expect("write");

        let config = mockls_multi_server_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers
            .iter()
            .map(|b| b.name.clone())
            .collect();

        let fs = test_fs();
        fs.set_roots(vec![root_a.path().to_path_buf()]);
        let manager = LspClientManager::new(config, test_logging(), fs);

        // Spawn both servers for root_a.
        for name in &bindings {
            manager
                .ensure_server(MOCK_LANG_A, name, root_a.path())
                .await?;
        }
        assert_eq!(manager.clients().await.len(), 2);

        // sync_roots adds root_b — both legacy servers should get root_b instances.
        manager
            .sync_roots(vec![
                root_a.path().to_path_buf(),
                root_b.path().to_path_buf(),
            ])
            .await?;

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 4, "2 legacy servers × 2 roots = 4 instances");
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 4);

        // Verify both roots are represented.
        let root_paths: HashSet<PathBuf> = clients
            .keys()
            .filter_map(|k| k.scope.root_path().map(Path::to_path_buf))
            .collect();
        assert!(root_paths.contains(root_a.path()));
        assert!(root_paths.contains(root_b.path()));
        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_remove_multi_server() -> Result<()> {
        // Removing a root shuts down per-root instances for all servers.
        let config = mockls_multi_server_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers
            .iter()
            .map(|b| b.name.clone())
            .collect();

        let manager = LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        // Spawn both servers for both roots (4 instances total).
        for name in &bindings {
            manager
                .ensure_server(MOCK_LANG_A, name, Path::new("/tmp"))
                .await?;
            manager.spawn(name, MOCK_LANG_A, Path::new("/var")).await?;
        }
        assert_eq!(manager.clients().await.len(), 4);

        // Remove /var — should shut down both servers' /var instances.
        manager.sync_roots(vec![PathBuf::from("/tmp")]).await?;

        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            2,
            "Only /tmp instances should remain after removing /var"
        );
        for key in clients.keys() {
            assert_eq!(
                key.scope,
                Scope::Root(PathBuf::from("/tmp")),
                "All remaining instances should be for /tmp"
            );
        }
        Ok(())
    }

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

    // --- Wait primitives (1c-02) ---

    #[tokio::test]
    async fn test_wait_ready_for_path_healthy() -> Result<()> {
        // Server reaches ready state: wait_ready_for_path returns.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        manager.wait_ready_for_path(&path).await;

        // If we got here, it didn't hang.
        Ok(())
    }

    #[tokio::test]
    async fn test_wait_ready_for_path_dead() -> Result<()> {
        // Server dies: wait_ready_for_path returns (doesn't hang).
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        // Kill the server.
        client.lock().await.shutdown().await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        manager.wait_ready_for_path(&path).await;

        // If we got here, dead server didn't block.
        Ok(())
    }

    #[tokio::test]
    async fn test_wait_ready_for_path_unrooted() {
        // File outside roots: returns immediately (no servers to wait for).
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let path = PathBuf::from(format!("/other/test.{MOCK_LANG_A}"));
        manager.wait_ready_for_path(&path).await;
    }

    #[tokio::test]
    async fn test_wait_ready_for_path_no_config() {
        // Unconfigured language: returns immediately.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        manager
            .wait_ready_for_path(Path::new("/tmp/test.xyz"))
            .await;
    }

    #[tokio::test]
    async fn test_wait_ready_all_mixed() -> Result<()> {
        // Some healthy, some dead: returns after all settle.
        let config = mockls_multi_server_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers
            .iter()
            .map(|b| b.name.clone())
            .collect();

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // Spawn both servers.
        let client_a = manager
            .ensure_server(MOCK_LANG_A, &bindings[0], Path::new("/tmp"))
            .await?;
        let _client_b = manager
            .ensure_server(MOCK_LANG_A, &bindings[1], Path::new("/tmp"))
            .await?;

        // Kill one server.
        client_a.lock().await.shutdown().await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        // wait_ready_all should still return (dead server doesn't block).
        manager.wait_ready_all().await;

        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_and_wait_for_paths() -> Result<()> {
        // Spawns new servers and returns after they're ready.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        assert!(manager.clients().await.is_empty());

        let paths = vec![PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"))];
        manager.ensure_and_wait_for_paths(&paths).await;

        assert!(
            has_language(&manager.clients().await, MOCK_LANG_A),
            "ensure_and_wait_for_paths should spawn the server"
        );
        Ok(())
    }

    // --- Document lifecycle (1c-03) ---

    #[tokio::test]
    async fn test_open_document_on_single_client() -> Result<()> {
        // open_document_on returns URI and sends didOpen.
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = test_fs_with_roots(&[]);
        fs.set_roots(vec![dir.path().to_path_buf()]);
        let manager = LspClientManager::new(mockls_config(), test_logging(), fs);

        let path = dir.path().join(format!("test.{MOCK_LANG_A}"));
        std::fs::write(&path, "content").expect("write");

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let uri = manager.open_document_on(&path, &client, None).await?;
        assert!(uri.starts_with("file://"));
        assert!(
            client.lock().await.is_document_open(&uri),
            "Client should track the document as open"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_open_document_on_second_call() -> Result<()> {
        // Second open on the same client sends didChange, not duplicate didOpen.
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = test_fs_with_roots(&[]);
        fs.set_roots(vec![dir.path().to_path_buf()]);
        let manager = LspClientManager::new(mockls_config(), test_logging(), fs);

        let path = dir.path().join(format!("test.{MOCK_LANG_A}"));
        std::fs::write(&path, "content").expect("write");

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let uri1 = manager.open_document_on(&path, &client, None).await?;
        let uri2 = manager.open_document_on(&path, &client, None).await?;
        assert_eq!(uri1, uri2);
        // Both calls succeed — second sends didChange since the client
        // already has the document open.
        assert!(client.lock().await.is_document_open(&uri1));
        Ok(())
    }

    #[tokio::test]
    async fn test_close_tracked_document() -> Result<()> {
        // close_tracked_document removes per-client tracking and sends didClose.
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = test_fs_with_roots(&[]);
        fs.set_roots(vec![dir.path().to_path_buf()]);
        let manager = LspClientManager::new(mockls_multi_server_config(), test_logging(), fs);

        let path = dir.path().join(format!("test.{MOCK_LANG_A}"));
        std::fs::write(&path, "content").expect("write");

        manager
            .ensure_clients_for_paths(std::slice::from_ref(&path))
            .await;

        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols)
            .await;
        assert_eq!(servers.len(), 2);

        // Open document on both servers.
        let mut uri = String::new();
        for c in &servers {
            uri = manager.open_document_on(&path, c, None).await?;
        }

        // Verify all clients have the document open.
        for c in &servers {
            assert!(c.lock().await.is_document_open(&uri));
        }

        // Close on each while holding the lock.
        for c in &servers {
            c.lock().await.close_tracked_document(&uri).await;
        }

        // Verify all clients no longer track the document.
        for c in &servers {
            assert!(
                !c.lock().await.is_document_open(&uri),
                "Document should be closed on all clients"
            );
        }
        Ok(())
    }

    // --- Project config infrastructure tests ---

    #[test]
    fn test_is_project_scoped_with_language() {
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());
        let root = PathBuf::from("/project");

        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            "rust".to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new("rust-analyzer")],
                ..LanguageConfig::default()
            },
        );
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root.clone(), pc);

        assert!(manager.is_project_scoped("rust", &root));
    }

    #[test]
    fn test_is_project_scoped_without_language() {
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());
        let root = PathBuf::from("/project");

        let mut pc = crate::config::ProjectConfig::default();
        pc.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                command: String::new(),
                args: Vec::new(),
                initialization_options: None,
                settings: Some(serde_json::json!({"key": "value"})),
                min_severity: None,
                file_patterns: Vec::new(),
                compiled_patterns: Vec::new(),
            },
        );
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root.clone(), pc);

        assert!(!manager.is_project_scoped("rust", &root));
    }

    #[test]
    fn test_is_project_scoped_no_config() {
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());
        let root = PathBuf::from("/project");

        assert!(!manager.is_project_scoped("rust", &root));
    }

    #[test]
    fn test_effective_server_def_merge() {
        let mut config = test_config();
        config.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                command: "rust-analyzer".to_string(),
                args: vec!["--log-level".to_string(), "info".to_string()],
                initialization_options: None,
                settings: Some(serde_json::json!({"check": {"command": "clippy"}, "cargo": {"features": ["a"]}})),
                min_severity: None,
                file_patterns: Vec::new(),
                compiled_patterns: Vec::new(),
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let root = PathBuf::from("/project");

        // Project config only overrides settings.
        let mut pc = crate::config::ProjectConfig::default();
        pc.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                command: String::new(), // empty = inherit from user
                args: Vec::new(),
                initialization_options: None,
                settings: Some(serde_json::json!({"check": {"command": "check"}, "new_key": true})),
                min_severity: None,
                file_patterns: Vec::new(),
                compiled_patterns: Vec::new(),
            },
        );
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root.clone(), pc);

        let merged = manager
            .effective_server_def("rust-analyzer", &root)
            .expect("should exist");

        // command/args inherited from user
        assert_eq!(merged.command, "rust-analyzer");
        assert_eq!(merged.args, vec!["--log-level", "info"]);

        // settings deep-merged
        let settings = merged.settings.expect("settings");
        assert_eq!(settings["check"]["command"], "check"); // project overrides
        assert_eq!(settings["cargo"]["features"], serde_json::json!(["a"])); // user preserved
        assert_eq!(settings["new_key"], true); // project adds
    }

    #[test]
    fn test_effective_server_def_full_override() {
        let mut config = test_config();
        config.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                command: "rust-analyzer".to_string(),
                args: vec!["--log-level".to_string(), "info".to_string()],
                initialization_options: None,
                settings: Some(serde_json::json!({"key": "user"})),
                min_severity: None,
                file_patterns: Vec::new(),
                compiled_patterns: Vec::new(),
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let root = PathBuf::from("/project");

        let mut pc = crate::config::ProjectConfig::default();
        pc.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                command: "custom-ra".to_string(),
                args: vec!["--custom".to_string()],
                initialization_options: None,
                settings: Some(serde_json::json!({"key": "project"})),
                min_severity: Some("warning".to_string()),
                file_patterns: Vec::new(),
                compiled_patterns: Vec::new(),
            },
        );
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root.clone(), pc);

        let merged = manager
            .effective_server_def("rust-analyzer", &root)
            .expect("should exist");

        assert_eq!(merged.command, "custom-ra");
        assert_eq!(merged.args, vec!["--custom"]);
        assert_eq!(merged.min_severity.as_deref(), Some("warning"));
        assert_eq!(merged.settings.expect("settings")["key"], "project");
    }

    #[test]
    fn test_effective_server_def_no_project() {
        let mut config = test_config();
        config.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                command: "rust-analyzer".to_string(),
                args: Vec::new(),
                initialization_options: None,
                settings: Some(serde_json::json!({"key": "user"})),
                min_severity: None,
                file_patterns: Vec::new(),
                compiled_patterns: Vec::new(),
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let root = PathBuf::from("/project");

        let def = manager
            .effective_server_def("rust-analyzer", &root)
            .expect("should exist");

        assert_eq!(def.command, "rust-analyzer");
        assert_eq!(def.settings.expect("settings")["key"], "user");
    }

    #[test]
    fn test_effective_settings_merge() {
        let mut config = test_config();
        config.server.insert(
            "ra".to_string(),
            ServerDef {
                command: "ra".to_string(),
                args: Vec::new(),
                initialization_options: None,
                settings: Some(serde_json::json!({"a": 1, "b": {"c": 2}})),
                min_severity: None,
                file_patterns: Vec::new(),
                compiled_patterns: Vec::new(),
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let root = PathBuf::from("/project");

        let mut pc = crate::config::ProjectConfig::default();
        pc.server.insert(
            "ra".to_string(),
            ServerDef {
                command: String::new(),
                args: Vec::new(),
                initialization_options: None,
                settings: Some(serde_json::json!({"b": {"d": 3}})),
                min_severity: None,
                file_patterns: Vec::new(),
                compiled_patterns: Vec::new(),
            },
        );
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root.clone(), pc);

        let settings = manager
            .effective_settings("ra", &root)
            .expect("should exist");
        assert_eq!(settings["a"], 1);
        assert_eq!(settings["b"]["c"], 2);
        assert_eq!(settings["b"]["d"], 3);
    }

    #[test]
    fn test_collect_per_root_settings() {
        let mut config = test_config();
        config.server.insert(
            "ra".to_string(),
            ServerDef {
                command: "ra".to_string(),
                args: Vec::new(),
                initialization_options: None,
                settings: Some(serde_json::json!({"base": true})),
                min_severity: None,
                file_patterns: Vec::new(),
                compiled_patterns: Vec::new(),
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let root_a = PathBuf::from("/project-a");
        let root_b = PathBuf::from("/project-b");

        // root_a has project config with settings for "ra"
        let mut pc_a = crate::config::ProjectConfig::default();
        pc_a.server.insert(
            "ra".to_string(),
            ServerDef {
                command: String::new(),
                args: Vec::new(),
                initialization_options: None,
                settings: Some(serde_json::json!({"override_a": true})),
                min_severity: None,
                file_patterns: Vec::new(),
                compiled_patterns: Vec::new(),
            },
        );
        // root_b has project config but no settings for "ra"
        let pc_b = crate::config::ProjectConfig::default();

        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root_a.clone(), pc_a);
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root_b.clone(), pc_b);

        let per_root = manager.collect_per_root_settings("ra", &[root_a.clone(), root_b]);
        // Only root_a has settings — root_b omitted
        assert_eq!(per_root.len(), 1);
        assert!(per_root.contains_key(&root_a));
        assert_eq!(per_root[&root_a]["override_a"], true);
    }

    #[test]
    fn test_collect_per_root_settings_no_configs() {
        let mut config = test_config();
        config.server.insert(
            "ra".to_string(),
            ServerDef {
                command: "ra".to_string(),
                args: Vec::new(),
                initialization_options: None,
                settings: None,
                min_severity: None,
                file_patterns: Vec::new(),
                compiled_patterns: Vec::new(),
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let per_root = manager.collect_per_root_settings("ra", &[PathBuf::from("/root")]);
        assert!(per_root.is_empty());
    }
}

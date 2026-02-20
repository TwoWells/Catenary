// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! IPC server for file-change notifications and root management.
//!
//! When Claude Code's native `Edit` or `Write` tools modify a file, a
//! `PostToolUse` hook runs `catenary notify`, which connects to this server
//! and sends the changed file path. The server notifies the LSP, waits for
//! fresh diagnostics, and returns them so they appear in the model's context.
//!
//! The server also accepts `sync_roots` requests from `catenary sync-roots`,
//! which synchronize workspace roots discovered from `/add-dir` and removal
//! commands in the Claude Code transcript. The older `add_roots` request type
//! is still supported for backwards compatibility.
//!
//! Transport: Unix domain sockets on Unix, named pipes on Windows.

use anyhow::{Result, anyhow};
use lsp_types::Diagnostic;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::bridge::{DocumentManager, DocumentNotification, PathValidator};
use crate::lsp::{ClientManager, DIAGNOSTICS_TIMEOUT, DiagnosticsWaitResult, LspClient};
use crate::session::{EventBroadcaster, EventKind};

/// Request from `catenary notify` (file change) or `catenary sync-roots` (root sync).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum NotifyRequest {
    /// A file-change notification.
    File {
        /// Absolute path to the changed file.
        file: String,
    },
    /// A request to synchronize workspace roots (full replacement).
    SyncRoots {
        /// Complete set of workspace roots — server diffs against current state.
        sync_roots: Vec<String>,
    },
    /// A request to add new workspace roots (incremental).
    AddRoots {
        /// Absolute paths of directories to add as roots.
        add_roots: Vec<String>,
    },
}

/// Listens on an IPC endpoint (Unix socket or named pipe) for file-change
/// notifications and returns LSP diagnostics.
pub struct NotifyServer {
    client_manager: Arc<ClientManager>,
    doc_manager: Arc<Mutex<DocumentManager>>,
    path_validator: Arc<RwLock<PathValidator>>,
    broadcaster: EventBroadcaster,
}

impl NotifyServer {
    /// Creates a new `NotifyServer`.
    #[must_use]
    pub const fn new(
        client_manager: Arc<ClientManager>,
        doc_manager: Arc<Mutex<DocumentManager>>,
        path_validator: Arc<RwLock<PathValidator>>,
        broadcaster: EventBroadcaster,
    ) -> Self {
        Self {
            client_manager,
            doc_manager,
            path_validator,
            broadcaster,
        }
    }

    /// Starts listening on the given IPC endpoint.
    ///
    /// Spawns a background task that accepts connections and processes
    /// file-change notifications. Returns a `JoinHandle` for the listener task.
    ///
    /// # Errors
    ///
    /// Returns an error if the endpoint cannot be created.
    #[cfg(unix)]
    pub fn start(self, socket_path: &std::path::Path) -> Result<tokio::task::JoinHandle<()>> {
        // Remove stale socket file if it exists
        let _ = std::fs::remove_file(socket_path);

        let listener = UnixListener::bind(socket_path).map_err(|e| {
            anyhow!(
                "Failed to bind notify socket {}: {e}",
                socket_path.display()
            )
        })?;

        info!("Notify socket listening on {}", socket_path.display());

        let server = Arc::new(self);

        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let server = server.clone();
                        tokio::spawn(async move {
                            if let Err(e) = server.handle_connection(stream).await {
                                debug!("Notify connection error: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        warn!("Notify socket accept error: {e}");
                    }
                }
            }
        });

        Ok(handle)
    }

    /// Starts listening on the given named pipe path.
    ///
    /// Spawns a background task that accepts connections and processes
    /// file-change notifications. Returns a `JoinHandle` for the listener task.
    ///
    /// # Errors
    ///
    /// Returns an error if the named pipe cannot be created.
    #[cfg(windows)]
    pub fn start(self, pipe_path: &std::path::Path) -> Result<tokio::task::JoinHandle<()>> {
        use tokio::net::windows::named_pipe::ServerOptions;

        let pipe_name = pipe_path.to_string_lossy().to_string();

        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)
            .map_err(|e| anyhow!("Failed to create notify pipe {pipe_name}: {e}"))?;

        info!("Notify pipe listening on {pipe_name}");

        let server_arc = Arc::new(self);

        let handle = tokio::spawn(async move {
            loop {
                // Wait for a client to connect to the current instance
                if let Err(e) = server.connect().await {
                    warn!("Notify pipe connect error: {e}");
                    continue;
                }

                let connected = server;

                // Create a fresh pipe instance before spawning the handler
                // so clients never see ERROR_FILE_NOT_FOUND
                server = match ServerOptions::new().create(&pipe_name) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("Notify pipe create error: {e}");
                        break;
                    }
                };

                let srv = server_arc.clone();
                tokio::spawn(async move {
                    if let Err(e) = srv.handle_connection(connected).await {
                        debug!("Notify connection error: {e}");
                    }
                });
            }
        });

        Ok(handle)
    }

    /// Handles a single connection: reads a JSON request, dispatches to the
    /// appropriate handler, and writes back the response.
    async fn handle_connection<S: AsyncRead + AsyncWrite + Unpin>(&self, stream: S) -> Result<()> {
        let (reader, mut writer) = tokio::io::split(stream);
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        buf_reader.read_line(&mut line).await?;

        let request: NotifyRequest =
            serde_json::from_str(line.trim()).map_err(|e| anyhow!("Invalid request: {e}"))?;

        let response = match request {
            NotifyRequest::File { file } => {
                debug!("Notify: processing file {file}");
                self.process_file(&file).await
            }
            NotifyRequest::SyncRoots { sync_roots } => {
                debug!("Notify: syncing {} root(s)", sync_roots.len());
                self.process_sync_roots(&sync_roots).await
            }
            NotifyRequest::AddRoots { add_roots } => {
                debug!("Notify: adding {} root(s)", add_roots.len());
                self.process_add_roots(&add_roots).await
            }
        };

        writer.write_all(response.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.shutdown().await?;

        Ok(())
    }

    /// Processes a file change notification and returns diagnostics text.
    async fn process_file(&self, file_path: &str) -> String {
        match self.process_file_inner(file_path).await {
            Ok(diagnostics) => diagnostics,
            Err(e) => format!("Notify error: {e}"),
        }
    }

    /// Inner implementation that can return errors.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "Locks held across async operations by design"
    )]
    async fn process_file_inner(&self, file_path: &str) -> Result<String> {
        let path = resolve_path(file_path)?;

        let canonical = self.path_validator.read().await.validate_read(&path)?;

        // Try to get the LSP client for this file's language
        let lang_id = {
            let doc_manager = self.doc_manager.lock().await;
            doc_manager.language_id_for_path(&canonical).to_string()
        };

        let client_mutex: Arc<Mutex<LspClient>> =
            match self.client_manager.get_client(&lang_id).await {
                Ok(c) => c,
                Err(_) => return Ok(String::new()), // No LSP server for this language
            };

        let mut doc_manager = self.doc_manager.lock().await;
        let client = client_mutex.lock().await;
        let lang = client.language().to_string();

        if !client.is_alive() {
            return Ok(format!(
                "[{lang}] server is not running \u{2014} diagnostics unavailable"
            ));
        }

        let uri = doc_manager.uri_for_path(&canonical)?;

        // ensure_open detects disk changes and returns didOpen/didChange
        if let Some(notification) = doc_manager.ensure_open(&canonical).await? {
            // Snapshot generation *before* sending the change
            let snapshot = client.diagnostics_generation(&uri).await;

            match notification {
                DocumentNotification::Open(params) => {
                    client.did_open(params).await?;
                }
                DocumentNotification::Change(params) => {
                    client.did_change(params).await?;
                }
            }

            // Trigger flycheck on servers that only run diagnostics on save
            client.did_save(uri.clone()).await?;

            drop(doc_manager);

            // Wait for diagnostics with retry-on-inactivity. If the server
            // goes silent (e.g., CPU-starved under load), re-send didSave
            // to nudge it back into action.
            let max_retries: u32 = 3;
            let mut server_died = false;

            for attempt in 0..max_retries {
                match client
                    .wait_for_diagnostics_update(&uri, snapshot, DIAGNOSTICS_TIMEOUT)
                    .await
                {
                    DiagnosticsWaitResult::Updated => break,
                    DiagnosticsWaitResult::ServerDied => {
                        server_died = true;
                        break;
                    }
                    DiagnosticsWaitResult::Inactive => {
                        if attempt + 1 < max_retries {
                            warn!(
                                "Server silent for {:?}, nudging with didSave (attempt {}/{})",
                                DIAGNOSTICS_TIMEOUT,
                                attempt + 2,
                                max_retries
                            );
                            client.did_save(uri.clone()).await?;
                        } else {
                            warn!(
                                "Server still silent after {} nudge attempts — \
                                 proceeding with cached diagnostics",
                                max_retries
                            );
                        }
                    }
                }
            }

            if server_died {
                return Ok(format!(
                    "[{lang}] server died \u{2014} diagnostics unavailable"
                ));
            }
        } else {
            drop(doc_manager);
        }

        let diagnostics = client.get_diagnostics(&uri).await;
        drop(client);

        let count = diagnostics.len();
        let compact = if diagnostics.is_empty() {
            String::new()
        } else {
            format_diagnostics_compact(&diagnostics)
        };

        // Broadcast diagnostics event for monitor visibility
        let preview = compact
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        self.broadcaster.send(EventKind::Diagnostics {
            file: file_path.to_string(),
            count,
            preview,
        });

        if diagnostics.is_empty() {
            Ok(String::new())
        } else {
            Ok(format!("Diagnostics ({count}):\n{compact}"))
        }
    }

    /// Synchronizes the full workspace root set.
    ///
    /// Canonicalizes incoming paths, diffs against the current root set, and
    /// applies both additions and removals. Uses `ClientManager::sync_roots()`
    /// to send a single `didChangeWorkspaceFolders` notification per LSP client.
    async fn process_sync_roots(&self, paths: &[String]) -> String {
        match self.process_sync_roots_inner(paths).await {
            Ok(msg) => msg,
            Err(e) => format!("Notify error: {e}"),
        }
    }

    /// Inner implementation for `process_sync_roots`.
    async fn process_sync_roots_inner(&self, paths: &[String]) -> Result<String> {
        let mut new_roots = Vec::new();
        for p in paths {
            let path = PathBuf::from(p);
            match path.canonicalize() {
                Ok(canonical) => {
                    if !new_roots.contains(&canonical) {
                        new_roots.push(canonical);
                    }
                }
                Err(e) => {
                    debug!("Skipping root {p}: {e}");
                }
            }
        }

        let current_roots = self.path_validator.read().await.roots().to_vec();

        // Check if anything actually changed
        if new_roots == current_roots {
            return Ok(String::new());
        }

        let added: Vec<String> = new_roots
            .iter()
            .filter(|r| !current_roots.contains(r))
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let removed: Vec<String> = current_roots
            .iter()
            .filter(|r| !new_roots.contains(r))
            .map(|p| p.to_string_lossy().into_owned())
            .collect();

        if added.is_empty() && removed.is_empty() {
            return Ok(String::new());
        }

        // Update path validator
        self.path_validator
            .write()
            .await
            .update_roots(new_roots.clone());

        // Sync LSP clients (handles both additions and removals)
        if let Err(e) = self.client_manager.sync_roots(new_roots).await {
            warn!("Failed to sync roots with LSP clients: {e}");
        }

        // Spawn any new language servers for added roots
        if !added.is_empty() {
            self.client_manager.spawn_all().await;
        }

        let mut parts = Vec::new();
        if !added.is_empty() {
            info!("Added roots: {}", added.join(", "));
            parts.push(format!("Added roots: {}", added.join(", ")));
        }
        if !removed.is_empty() {
            info!("Removed roots: {}", removed.join(", "));
            parts.push(format!("Removed roots: {}", removed.join(", ")));
        }
        Ok(parts.join("\n"))
    }

    /// Processes a request to add new workspace roots.
    ///
    /// Canonicalizes each path, filters to genuinely new roots, updates the
    /// path validator, notifies LSP clients, and spawns servers for new languages.
    async fn process_add_roots(&self, paths: &[String]) -> String {
        match self.process_add_roots_inner(paths).await {
            Ok(msg) => msg,
            Err(e) => format!("Notify error: {e}"),
        }
    }

    /// Inner implementation for `process_add_roots`.
    async fn process_add_roots_inner(&self, paths: &[String]) -> Result<String> {
        // Canonicalize each path, skipping any that don't exist
        let mut new_paths = Vec::new();
        for p in paths {
            let path = PathBuf::from(p);
            match path.canonicalize() {
                Ok(canonical) => new_paths.push(canonical),
                Err(e) => {
                    debug!("Skipping root {p}: {e}");
                }
            }
        }

        if new_paths.is_empty() {
            return Ok(String::new());
        }

        // Get current roots and filter to only genuinely new ones
        let current_roots = self.path_validator.read().await.roots().to_vec();
        let genuinely_new: Vec<PathBuf> = new_paths
            .into_iter()
            .filter(|p| !current_roots.contains(p))
            .collect();

        if genuinely_new.is_empty() {
            return Ok(String::new());
        }

        // Build new full root list
        let mut all_roots = current_roots;
        all_roots.extend(genuinely_new.iter().cloned());

        // Update path validator
        self.path_validator.write().await.update_roots(all_roots);

        // Notify LSP clients about each new root
        for root in &genuinely_new {
            if let Err(e) = self.client_manager.add_root(root.clone()).await {
                warn!("Failed to add root {}: {e}", root.display());
            }
        }

        // Spawn any new language servers for the added roots
        self.client_manager.spawn_all().await;

        let added: Vec<String> = genuinely_new
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        info!("Added roots: {}", added.join(", "));
        Ok(format!("Added roots: {}", added.join(", ")))
    }
}

/// Resolves a file path to an absolute path.
fn resolve_path(file: &str) -> Result<PathBuf> {
    let path = PathBuf::from(file);
    if path.is_absolute() {
        Ok(path)
    } else {
        let cwd = std::env::current_dir()
            .map_err(|e| anyhow!("Failed to get current working directory: {e}"))?;
        Ok(cwd.join(path))
    }
}

/// Formats diagnostics with line/column and severity.
pub(crate) fn format_diagnostics_compact(diagnostics: &[Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(|d| {
            let severity = match d.severity {
                Some(lsp_types::DiagnosticSeverity::ERROR) => "error",
                Some(lsp_types::DiagnosticSeverity::WARNING) => "warning",
                Some(lsp_types::DiagnosticSeverity::INFORMATION) => "info",
                Some(lsp_types::DiagnosticSeverity::HINT) => "hint",
                _ => "unknown",
            };
            let line = d.range.start.line + 1;
            let col = d.range.start.character + 1;
            let source = d.source.as_deref().unwrap_or("");
            let code = d
                .code
                .as_ref()
                .map(|c| match c {
                    lsp_types::NumberOrString::Number(n) => n.to_string(),
                    lsp_types::NumberOrString::String(s) => s.clone(),
                })
                .unwrap_or_default();

            if code.is_empty() {
                format!("  {line}:{col} [{severity}] {source}: {}", d.message)
            } else {
                format!(
                    "  {line}:{col} [{severity}] {source}({code}): {}",
                    d.message
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

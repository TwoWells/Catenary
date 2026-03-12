// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! IPC server for host CLI hook integration.
//!
//! When a host CLI's post-tool hook fires after a file edit, it runs
//! `catenary hook post-tool`, which connects to this server and sends the
//! changed file path. The server notifies the LSP, waits for fresh
//! diagnostics, and returns them so they appear in the model's context.
//!
//! The server also accepts `sync_roots` requests from `catenary hook
//! pre-tool`, which synchronize workspace roots discovered from `/add-dir`
//! and removal commands in the Claude Code transcript. The older `add_roots`
//! request type is still supported for backwards compatibility.
//!
//! Transport: Unix domain sockets on Unix, named pipes on Windows.

use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::bridge::{DocumentManager, DocumentNotification, PathValidator};
use crate::lsp::{ClientManager, DiagnosticsWaitResult, LspClient};
use crate::session::{EventBroadcaster, EventKind, MessageLog};

/// Request from `catenary hook PostToolUse` (file change) or `catenary hook PreToolUse` (root sync).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum NotifyRequest {
    /// A file-change notification.
    File {
        /// Absolute path to the changed file.
        file: String,
        /// Name of the host CLI tool that triggered the hook (e.g., "Write", "Edit").
        /// Included in the logged payload for monitor visibility.
        #[serde(default)]
        #[allow(
            dead_code,
            reason = "metadata field — logged in payload, not read in code"
        )]
        tool: Option<String>,
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

/// IPC response from the hook server to the CLI.
///
/// Separates diagnostic content (for the model via `additionalContext`) from
/// internal errors (for the user via `systemMessage`). The CLI deserializes
/// this to decide where to route the output.
#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotifyResult {
    /// Diagnostic content for the model (may be `[clean]`, `[no language server]`,
    /// `[diagnostics unavailable]`, or formatted diagnostic lines).
    Content(String),
    /// Internal error for the user (path resolution, LSP client failures, etc.).
    Error(String),
}

/// Listens on an IPC endpoint (Unix socket or named pipe) for hook requests
/// from the host CLI and returns LSP diagnostics or root sync results.
pub struct HookServer {
    client_manager: Arc<ClientManager>,
    doc_manager: Arc<Mutex<DocumentManager>>,
    path_validator: Arc<RwLock<PathValidator>>,
    broadcaster: EventBroadcaster,
    message_log: Arc<MessageLog>,
    client_name: String,
}

impl HookServer {
    /// Creates a new `HookServer`.
    #[must_use]
    pub const fn new(
        client_manager: Arc<ClientManager>,
        doc_manager: Arc<Mutex<DocumentManager>>,
        path_validator: Arc<RwLock<PathValidator>>,
        broadcaster: EventBroadcaster,
        message_log: Arc<MessageLog>,
        client_name: String,
    ) -> Self {
        Self {
            client_manager,
            doc_manager,
            path_validator,
            broadcaster,
            message_log,
            client_name,
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

        let method = match &request {
            NotifyRequest::File { .. } => "post-tool",
            NotifyRequest::SyncRoots { .. } | NotifyRequest::AddRoots { .. } => "pre-tool",
        };

        // Log incoming hook request
        let entry_id = self.message_log.log(
            "hook",
            method,
            "catenary",
            &self.client_name,
            None,
            None,
            &serde_json::from_str::<Value>(line.trim()).unwrap_or_default(),
        );

        let response = match request {
            NotifyRequest::File { file, .. } => {
                debug!("Hook: processing file {file}");
                self.process_file(&file, entry_id).await
            }
            NotifyRequest::SyncRoots { sync_roots } => {
                debug!("Hook: syncing {} root(s)", sync_roots.len());
                self.process_sync_roots(&sync_roots).await
            }
            NotifyRequest::AddRoots { add_roots } => {
                debug!("Hook: adding {} root(s)", add_roots.len());
                self.process_add_roots(&add_roots).await
            }
        };

        // Log outgoing hook response
        self.message_log.log(
            "hook",
            method,
            "catenary",
            &self.client_name,
            Some(entry_id),
            None,
            &serde_json::from_str::<Value>(&response).unwrap_or_default(),
        );

        writer.write_all(response.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.shutdown().await?;

        Ok(())
    }

    /// Processes a file change notification and returns a [`NotifyResult`] as JSON.
    async fn process_file(&self, file_path: &str, entry_id: i64) -> String {
        let result = match self.process_file_inner(file_path, entry_id).await {
            Ok(content) => NotifyResult::Content(content),
            Err(e) => {
                warn!("Notify error for {file_path}: {e}");
                NotifyResult::Error(e.to_string())
            }
        };
        // Safe: NotifyResult serialization cannot fail (no non-string map keys, no floats).
        serde_json::to_string(&result).unwrap_or_default()
    }

    /// Inner implementation that can return errors.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "Locks held across async operations by design"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "Diagnostics wait loop adds necessary branches"
    )]
    async fn process_file_inner(&self, file_path: &str, entry_id: i64) -> Result<String> {
        let path = resolve_path(file_path)?;

        // Gate on workspace roots: if the LSP server doesn't know about this
        // file's directory, asking for diagnostics is a wasted round-trip.
        let canonical = self.path_validator.read().await.validate_read(&path)?;

        // Try to get the LSP client for this file's language
        let lang_id = {
            let doc_manager = self.doc_manager.lock().await;
            doc_manager.language_id_for_path(&canonical).to_string()
        };

        let client_mutex: Arc<Mutex<LspClient>> = match self
            .client_manager
            .get_client_for_path(&canonical, &lang_id)
            .await
        {
            Ok(c) => c,
            Err(_) => return Ok("[no language server]".into()),
        };

        let mut doc_manager = self.doc_manager.lock().await;
        let mut client = client_mutex.lock().await;

        // Thread parent_id so LSP requests are correlated with this hook
        client.set_parent_id(Some(entry_id));

        if !client.is_alive() {
            client.set_parent_id(None);
            return Ok("[no language server]".into());
        }

        let uri = doc_manager.uri_for_path(&canonical)?;

        // ensure_open detects disk changes and returns didOpen/didChange
        if let Some(notification) = doc_manager.ensure_open(&canonical).await? {
            // Snapshot generation *before* sending the change
            let snapshot = client.diagnostics_generation(&uri);

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

            // Trigger flycheck on servers that only run diagnostics on save
            if client.wants_did_save() {
                client.did_save(&uri).await?;
            }

            drop(doc_manager);

            if client.wait_for_diagnostics_update(&uri, snapshot).await
                == DiagnosticsWaitResult::Nothing
            {
                client.set_parent_id(None);
                return Ok("[diagnostics unavailable]".into());
            }
        } else {
            drop(doc_manager);
        }

        let diagnostics = client.get_diagnostics(&uri);

        // Extract filter context before dropping the client lock
        let server_command = client.server_command().to_string();
        let server_version = client.server_version().map(str::to_string);

        // Collect quick-fix code actions for each diagnostic
        let fixes = if !diagnostics.is_empty()
            && client
                .capabilities()
                .get("codeActionProvider")
                .is_some_and(|v| !v.is_null())
        {
            collect_quick_fixes(&client, &uri, &diagnostics).await
        } else {
            Vec::new()
        };

        client.set_parent_id(None);
        drop(client);

        // Apply severity threshold from config
        let min_severity = self
            .client_manager
            .config()
            .server
            .get(&lang_id)
            .and_then(|sc| sc.min_severity.as_deref())
            .and_then(crate::filter::parse_severity);

        let (diagnostics, fixes) = if let Some(threshold) = min_severity {
            let mut filtered_diags = Vec::new();
            let mut filtered_fixes = Vec::new();
            for (diag, fix) in diagnostics
                .into_iter()
                .zip(fixes.into_iter().chain(std::iter::repeat_with(Vec::new)))
            {
                if let Some(sev) = crate::lsp::extract::diagnostic_severity(&diag) {
                    if crate::filter::severity_passes(sev, threshold) {
                        filtered_diags.push(diag);
                        filtered_fixes.push(fix);
                    }
                } else {
                    // No severity = pass through
                    filtered_diags.push(diag);
                    filtered_fixes.push(fix);
                }
            }
            (filtered_diags, filtered_fixes)
        } else {
            (diagnostics, fixes)
        };

        let filter = crate::filter::get_filter(&server_command);

        let count = diagnostics.len();
        let compact = if diagnostics.is_empty() {
            String::new()
        } else {
            format_diagnostics_compact(
                &diagnostics,
                &fixes,
                filter,
                &server_command,
                server_version.as_deref(),
                &lang_id,
            )
        };

        // Broadcast diagnostics event for monitor visibility
        let preview = compact.clone();
        self.broadcaster.send(EventKind::Diagnostics {
            file: file_path.to_string(),
            count,
            preview,
        });

        if diagnostics.is_empty() {
            Ok("[clean]".into())
        } else {
            Ok(compact)
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

/// Collects quick-fix titles for each diagnostic from the LSP server.
///
/// Returns a `Vec` parallel to `diagnostics` — each entry contains the
/// titles of quick-fix code actions for that diagnostic. Diagnostics
/// without fixes get an empty vec.
///
/// Requests are dispatched concurrently via `futures::future::join_all`
/// to avoid sequential per-diagnostic latency (25-30 diagnostics is
/// common in real-world files).
async fn collect_quick_fixes(
    client: &LspClient,
    uri: &str,
    diagnostics: &[Value],
) -> Vec<Vec<String>> {
    let futures: Vec<_> = diagnostics
        .iter()
        .map(|diag| async move {
            let Some(range) = crate::lsp::extract::diagnostic_range(diag) else {
                return Vec::new();
            };
            let diag_slice = [diag.clone()];
            client
                .code_action(
                    uri,
                    range.start.line,
                    range.start.character,
                    range.end.line,
                    range.end.character,
                    &diag_slice,
                )
                .await
                .map_or_else(
                    |_| Vec::new(),
                    |result| {
                        result
                            .as_array()
                            .map(|actions| {
                                actions
                                    .iter()
                                    .filter_map(|a| {
                                        if a.get("kind").and_then(Value::as_str) == Some("quickfix")
                                        {
                                            a.get("title")
                                                .and_then(Value::as_str)
                                                .map(str::to_string)
                                        } else {
                                            None
                                        }
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    },
                )
        })
        .collect();

    futures::future::join_all(futures).await
}

/// Formats diagnostics with line/column, severity, and optional quick-fix titles.
///
/// `fixes` is parallel to `diagnostics` — each entry contains the titles of
/// quick-fix code actions for that diagnostic. Pass an empty slice when no
/// fixes were collected.
///
/// Messages are passed through the provided [`DiagnosticFilter`] for noise
/// stripping. Diagnostics whose filtered message is empty are dropped.
pub(crate) fn format_diagnostics_compact(
    diagnostics: &[Value],
    fixes: &[Vec<String>],
    filter: &dyn crate::filter::DiagnosticFilter,
    server_command: &str,
    server_version: Option<&str>,
    language_id: &str,
) -> String {
    diagnostics
        .iter()
        .enumerate()
        .filter_map(|(i, d)| {
            let severity = match crate::lsp::extract::diagnostic_severity(d) {
                Some(1) => "error",
                Some(2) => "warning",
                Some(3) => "info",
                Some(4) => "hint",
                _ => "unknown",
            };
            let (line, col) = crate::lsp::extract::diagnostic_range(d)
                .map_or((0, 0), |r| (r.start.line + 1, r.start.character + 1));
            let source = d.get("source").and_then(Value::as_str);
            let source_str = source.unwrap_or("");
            let code_value = d.get("code");
            let code = code_value
                .map(|c| {
                    c.as_i64().map_or_else(
                        || c.as_str().map_or_else(|| c.to_string(), str::to_string),
                        |n| n.to_string(),
                    )
                })
                .unwrap_or_default();

            let diag_code = code_value.map(crate::filter::DiagnosticCode::from_value);
            let message = filter.filter_message(
                server_command,
                server_version,
                source,
                diag_code.as_ref(),
                crate::lsp::extract::diagnostic_severity(d)
                    .unwrap_or(crate::filter::SEVERITY_WARNING),
                language_id,
                crate::lsp::extract::diagnostic_message(d).unwrap_or(""),
            );

            // Empty message means the filter wants to drop this diagnostic
            if message.is_empty() {
                return None;
            }

            let mut result = if code.is_empty() {
                format!("\t:{line}:{col} [{severity}] {source_str}: {message}")
            } else {
                format!("\t:{line}:{col} [{severity}] {source_str}({code}): {message}")
            };

            // Append indented fix lines
            if let Some(fix_titles) = fixes.get(i) {
                for title in fix_titles {
                    use std::fmt::Write;
                    let _ = write!(result, "\n\t\tfix: {title}");
                }
            }

            Some(result)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn notify_result_content_round_trip() {
        let original = NotifyResult::Content("[clean]".into());
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: NotifyResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);

        let raw: serde_json::Value = serde_json::from_str(&json).expect("parse as value");
        assert_eq!(raw["content"], "[clean]");
        assert!(raw.get("error").is_none());
    }

    #[test]
    fn notify_result_error_round_trip() {
        let original = NotifyResult::Error("path resolution failed".into());
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: NotifyResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);

        let raw: serde_json::Value = serde_json::from_str(&json).expect("parse as value");
        assert_eq!(raw["error"], "path resolution failed");
        assert!(raw.get("content").is_none());
    }

    #[test]
    fn test_hook_request_tool_field() {
        // With tool field
        let json = r#"{"file": "/tmp/test.rs", "tool": "Write"}"#;
        let req: NotifyRequest = serde_json::from_str(json).expect("deserialize with tool");
        let NotifyRequest::File { file, tool } = req else {
            unreachable!("expected File variant");
        };
        assert_eq!(file, "/tmp/test.rs");
        assert_eq!(tool.as_deref(), Some("Write"));

        // Without tool field (backward compatibility)
        let json = r#"{"file": "/tmp/test.rs"}"#;
        let req: NotifyRequest = serde_json::from_str(json).expect("deserialize without tool");
        let NotifyRequest::File { tool, .. } = req else {
            unreachable!("expected File variant");
        };
        assert!(tool.is_none());
    }

    #[test]
    fn test_hook_log_file_request() {
        let (_dir, _path, conn) = test_db();
        let conn = Arc::new(std::sync::Mutex::new(conn));

        // Insert a session for the FK.
        conn.lock()
            .expect("lock")
            .execute(
                "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert session");

        let log = Arc::new(MessageLog::new(conn.clone(), "s1".to_string()));

        // Simulate what handle_connection does for a File request
        let method = "post-tool";
        let request_payload = serde_json::json!({"file": "/tmp/test.rs", "tool": "Write"});
        let entry_id = log.log(
            "hook",
            method,
            "catenary",
            "claude-code",
            None,
            None,
            &request_payload,
        );
        assert!(entry_id > 0);

        let response_payload = serde_json::json!({"content": "[clean]"});
        let resp_id = log.log(
            "hook",
            method,
            "catenary",
            "claude-code",
            Some(entry_id),
            None,
            &response_payload,
        );
        assert!(resp_id > entry_id);

        // Verify both messages in the database
        let (r_type, r_method): (String, String) = conn
            .lock()
            .expect("lock")
            .query_row(
                "SELECT type, method FROM messages WHERE id = ?1",
                [entry_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("query request");
        assert_eq!(r_type, "hook");
        assert_eq!(r_method, "post-tool");

        let stored_req_id: Option<i64> = conn
            .lock()
            .expect("lock")
            .query_row(
                "SELECT request_id FROM messages WHERE id = ?1",
                [resp_id],
                |row| row.get(0),
            )
            .expect("query response");
        assert_eq!(stored_req_id, Some(entry_id));
    }

    #[test]
    fn test_hook_log_sync_roots() {
        let (_dir, _path, conn) = test_db();
        let conn = Arc::new(std::sync::Mutex::new(conn));

        conn.lock()
            .expect("lock")
            .execute(
                "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert session");

        let log = Arc::new(MessageLog::new(conn.clone(), "s1".to_string()));

        let method = "pre-tool";
        let request_payload = serde_json::json!({"sync_roots": ["/tmp/root1"]});
        let entry_id = log.log(
            "hook",
            method,
            "catenary",
            "host",
            None,
            None,
            &request_payload,
        );

        let response_payload = serde_json::json!("Added roots: /tmp/root1");
        log.log(
            "hook",
            method,
            "catenary",
            "host",
            Some(entry_id),
            None,
            &response_payload,
        );

        // Verify method is pre-tool
        let r_method: String = conn
            .lock()
            .expect("lock")
            .query_row(
                "SELECT method FROM messages WHERE id = ?1",
                [entry_id],
                |row| row.get(0),
            )
            .expect("query");
        assert_eq!(r_method, "pre-tool");
    }

    /// Open an isolated test database in a tempdir.
    fn test_db() -> (tempfile::TempDir, std::path::PathBuf, rusqlite::Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catenary").join("catenary.db");
        let conn = crate::db::open_and_migrate_at(&path).expect("open test DB");
        (dir, path, conn)
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Unix socket server for file-change notifications.
//!
//! When Claude Code's native `Edit` or `Write` tools modify a file, a
//! `PostToolUse` hook runs `catenary notify`, which connects to this socket
//! and sends the changed file path. The server notifies the LSP, waits for
//! fresh diagnostics, and returns them so they appear in the model's context.

use anyhow::{Result, anyhow};
use lsp_types::Diagnostic;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::bridge::{DocumentManager, DocumentNotification, PathValidator};
use crate::lsp::{ClientManager, DIAGNOSTICS_TIMEOUT, LspClient};

/// Request from the `catenary notify` CLI.
#[derive(Debug, Deserialize)]
struct NotifyRequest {
    /// Absolute path to the changed file.
    file: String,
}

/// Listens on a Unix socket for file-change notifications and returns
/// LSP diagnostics.
pub struct NotifyServer {
    client_manager: Arc<ClientManager>,
    doc_manager: Arc<Mutex<DocumentManager>>,
    path_validator: Arc<RwLock<PathValidator>>,
}

impl NotifyServer {
    /// Creates a new `NotifyServer`.
    #[must_use]
    pub const fn new(
        client_manager: Arc<ClientManager>,
        doc_manager: Arc<Mutex<DocumentManager>>,
        path_validator: Arc<RwLock<PathValidator>>,
    ) -> Self {
        Self {
            client_manager,
            doc_manager,
            path_validator,
        }
    }

    /// Starts listening on the given Unix socket path.
    ///
    /// Spawns a background task that accepts connections and processes
    /// file-change notifications. Returns a `JoinHandle` for the listener task.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket cannot be bound.
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

    /// Handles a single connection: reads a JSON request, processes the
    /// file change, and writes back diagnostics.
    async fn handle_connection(&self, stream: tokio::net::UnixStream) -> Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        buf_reader.read_line(&mut line).await?;

        let request: NotifyRequest =
            serde_json::from_str(line.trim()).map_err(|e| anyhow!("Invalid request: {e}"))?;

        debug!("Notify: processing {}", request.file);

        let response = self.process_file(&request.file).await;

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

            // Wait for diagnostics that reflect our change
            if !client
                .wait_for_diagnostics_update(&uri, snapshot, DIAGNOSTICS_TIMEOUT)
                .await
            {
                return Ok(format!(
                    "[{lang}] server stopped responding \u{2014} diagnostics unavailable"
                ));
            }
        } else {
            drop(doc_manager);
        }

        let diagnostics = client.get_diagnostics(&uri).await;
        drop(client);

        if diagnostics.is_empty() {
            Ok(String::new())
        } else {
            Ok(format!(
                "Diagnostics ({}):\n{}",
                diagnostics.len(),
                format_diagnostics_compact(&diagnostics)
            ))
        }
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

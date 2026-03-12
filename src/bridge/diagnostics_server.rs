// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Diagnostics pipeline for PostToolUse hook requests.
//!
//! Handles file-change notifications: path resolution, LSP client lookup,
//! document open/change, diagnostics wait, severity filtering, noise
//! filtering, quick-fix collection, and compact formatting.

use super::path_security::PathValidator;
use super::tool_server::ToolServer;
use super::{DocumentManager, DocumentNotification};
use crate::lsp::{ClientManager, DiagnosticsWaitResult, LspClient};
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

/// Result of processing a file change through the diagnostics pipeline.
pub struct DiagnosticsResult {
    /// Status text for the hook response (`[clean]`, diagnostics text, etc.).
    pub content: String,
    /// Number of diagnostics found.
    pub count: usize,
}

/// Handles `PostToolUse` hook requests: file-change notification with LSP
/// diagnostics collection and formatting.
pub struct DiagnosticsServer {
    client_manager: Arc<ClientManager>,
    doc_manager: Arc<Mutex<DocumentManager>>,
    path_validator: Arc<RwLock<PathValidator>>,
}

impl DiagnosticsServer {
    /// Creates a new `DiagnosticsServer`.
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

    /// Processes a file change and returns diagnostics.
    ///
    /// # Errors
    ///
    /// Returns an error if path resolution, LSP client lookup, or document
    /// sync fails.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "Locks held across async operations by design"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "Diagnostics wait loop adds necessary branches"
    )]
    pub async fn process_file(&self, file_path: &str, entry_id: i64) -> Result<DiagnosticsResult> {
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
            Err(_) => {
                return Ok(DiagnosticsResult {
                    content: "[no language server]".into(),
                    count: 0,
                });
            }
        };

        let mut doc_manager = self.doc_manager.lock().await;
        let mut client = client_mutex.lock().await;

        // Thread parent_id so LSP requests are correlated with this hook
        client.set_parent_id(Some(entry_id));

        if !client.is_alive() {
            client.set_parent_id(None);
            return Ok(DiagnosticsResult {
                content: "[no language server]".into(),
                count: 0,
            });
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
                return Ok(DiagnosticsResult {
                    content: "[diagnostics unavailable]".into(),
                    count: 0,
                });
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
        let content = if diagnostics.is_empty() {
            "[clean]".into()
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

        Ok(DiagnosticsResult { content, count })
    }

    /// Processes multiple file changes and returns a combined diagnostics string.
    ///
    /// Runs the full pipeline for each file (document sync, wait, severity
    /// filtering, noise filtering, quick-fixes). Files with `[clean]` or
    /// `[diagnostics unavailable]` results are omitted. Errors are
    /// best-effort skipped.
    pub async fn process_files(&self, files: &[&str], entry_id: i64) -> String {
        use std::fmt::Write;

        let mut output = String::new();

        for &file in files {
            let Ok(result) = self.process_file(file, entry_id).await else {
                continue;
            };

            if result.content.is_empty()
                || result.content == "[clean]"
                || result.content == "[diagnostics unavailable]"
                || result.content == "[no language server]"
            {
                continue;
            }

            if output.is_empty() {
                output.push_str("diagnostics:\n");
            }
            _ = writeln!(output, "\t{file}");
            for line in result.content.lines() {
                _ = writeln!(output, "\t{line}");
            }
        }

        output
    }
}

impl ToolServer for DiagnosticsServer {
    async fn execute(
        &self,
        params: &serde_json::Value,
        parent_id: Option<i64>,
    ) -> Result<serde_json::Value> {
        let file = params
            .get("file")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("missing \"file\" parameter"))?;

        let entry_id = parent_id.unwrap_or(0);
        let result = self.process_file(file, entry_id).await?;

        Ok(serde_json::json!({
            "content": result.content,
            "count": result.count,
        }))
    }
}

/// Resolves a file path to an absolute path.
pub(crate) fn resolve_path(file: &str) -> Result<PathBuf> {
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

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Diagnostics pipeline for PostToolUse hook requests.
//!
//! Handles file-change notifications: path resolution, LSP client lookup,
//! document open/change, idle detection, diagnostics retrieval (push cache
//! first, pull fallback), severity filtering, noise filtering, quick-fix
//! collection, and compact formatting.

use super::path_security::PathValidator;
use super::tool_server::ToolServer;
use crate::lsp::settle::{IdleDetector, SettleResult, await_idle};
use crate::lsp::state::ServerLifecycle;
use crate::lsp::{LspClient, LspClientManager};
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Result of processing a file change through the diagnostics pipeline.
pub struct DiagnosticsResult {
    /// Status text for the hook response (`[clean]`, diagnostics text, etc.).
    pub content: String,
    /// Number of diagnostics found.
    pub count: usize,
}

/// Per-server diagnostics result from [`DiagnosticsServer::process_file_on_server`].
struct ServerDiagnostics {
    /// Formatted diagnostic output for this server (empty if clean).
    formatted: String,
    /// Number of diagnostics from this server.
    count: usize,
}

impl ServerDiagnostics {
    /// Returns an empty result (server died or was skipped).
    const fn empty() -> Self {
        Self {
            formatted: String::new(),
            count: 0,
        }
    }
}

/// Handles `PostToolUse` hook requests: file-change notification with LSP
/// diagnostics collection and formatting.
pub struct DiagnosticsServer {
    client_manager: Arc<LspClientManager>,
    path_validator: Arc<RwLock<PathValidator>>,
}

impl DiagnosticsServer {
    /// Creates a new `DiagnosticsServer`.
    pub const fn new(
        client_manager: Arc<LspClientManager>,
        path_validator: Arc<RwLock<PathValidator>>,
    ) -> Self {
        Self {
            client_manager,
            path_validator,
        }
    }

    /// Processes a file change and returns diagnostics from all
    /// diagnostic-enabled servers.
    ///
    /// Pipeline: path resolution → server selection → per-server
    /// open + settle + retrieve + filter + close → concatenate.
    ///
    /// Each server's lifecycle is self-contained: if one server fails
    /// at any stage (open, settle, retrieval), it is skipped and the
    /// remaining servers still contribute diagnostics.
    ///
    /// # Errors
    ///
    /// Returns an error if path resolution fails.
    pub async fn process_file(&self, file_path: &str, entry_id: i64) -> Result<DiagnosticsResult> {
        let path = resolve_path(file_path)?;

        // Gate on workspace roots: if the LSP server doesn't know about this
        // file's directory, asking for diagnostics is a wasted round-trip.
        let canonical = self.path_validator.read().await.validate_read(&path)?;

        // Get diagnostic-enabled servers without opening documents.
        let clients = self.client_manager.diagnostic_servers(&canonical).await;

        if clients.is_empty() {
            return Ok(DiagnosticsResult {
                content: "[no language server]".into(),
                count: 0,
            });
        }

        // Run full pipeline per server (open → settle → retrieve → close).
        let mut all_segments: Vec<String> = Vec::new();
        let mut total_count = 0;

        for client_mutex in &clients {
            let segment = self
                .process_file_on_server(client_mutex, &canonical, Some(entry_id))
                .await;
            total_count += segment.count;
            if !segment.formatted.is_empty() {
                all_segments.push(segment.formatted);
            }
        }

        let content = if total_count == 0 {
            "[clean]".into()
        } else {
            all_segments.join("\n")
        };

        Ok(DiagnosticsResult {
            content,
            count: total_count,
        })
    }

    /// Runs the full diagnostics pipeline on a single server.
    ///
    /// Opens the document, settles the server, retrieves diagnostics
    /// (push cache first, pull fallback), applies per-server
    /// `min_severity` filtering and noise filtering, collects
    /// quick-fixes, formats the output, and closes the document.
    ///
    /// Returns empty results on any failure — errors are logged via
    /// `warn!()` and never reach the agent-facing tool result.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "Client lock held across pipeline for exclusive access"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "Pipeline steps are sequential and cannot be split"
    )]
    async fn process_file_on_server(
        &self,
        client_mutex: &Arc<Mutex<LspClient>>,
        path: &std::path::Path,
        parent_id: Option<i64>,
    ) -> ServerDiagnostics {
        let empty = ServerDiagnostics::empty();

        // Open document on this server. If the server died or the open
        // fails for any reason, skip it — other servers still contribute.
        let uri = match self
            .client_manager
            .open_document_on(path, client_mutex, parent_id)
            .await
        {
            Ok(uri) => uri,
            Err(e) => {
                let name = client_mutex.lock().await.server_name().to_string();
                warn!(
                    server = %name,
                    "document open failed, skipping server: {e}",
                );
                return empty;
            }
        };

        let result = self.run_diagnostics_pipeline(client_mutex, &uri).await;

        // Always close and clear parent_id, even on pipeline failure.
        self.client_manager.close_document(&uri, client_mutex).await;
        client_mutex.lock().await.set_parent_id(None);

        result
    }

    /// Diagnostics pipeline after document open: settle → retrieve →
    /// filter → format.
    ///
    /// Extracted so that [`Self::process_file_on_server`] always runs
    /// the close path regardless of pipeline outcome.
    #[allow(
        clippy::too_many_lines,
        reason = "Pipeline steps are sequential and cannot be split"
    )]
    async fn run_diagnostics_pipeline(
        &self,
        client_mutex: &Arc<Mutex<LspClient>>,
        uri: &str,
    ) -> ServerDiagnostics {
        let empty = ServerDiagnostics::empty();

        let client = client_mutex.lock().await;
        let server_name = client.server_name().to_string();

        // Check lifecycle before settling
        if matches!(
            client.lifecycle(),
            ServerLifecycle::Failed | ServerLifecycle::Dead
        ) {
            return empty;
        }

        let server = client.server().clone();
        let cancel = CancellationToken::new();

        // Idle wait: ensure server has settled — covers processing of the
        // didOpen/didChange sent by open_document_on.
        let pre_detector = IdleDetector::unconditional();
        let pre_result = await_idle(&server, pre_detector, cancel.clone()).await;
        debug!(
            server = %server_name,
            "idle result: {pre_result:?}",
        );

        if pre_result == SettleResult::RootDied
            || matches!(
                client.lifecycle(),
                ServerLifecycle::Failed | ServerLifecycle::Dead
            )
        {
            return empty;
        }

        // Health probe: verify the server can respond before continuing
        if client.lifecycle() == ServerLifecycle::Probing && !client.run_health_probe(uri).await {
            return empty;
        }

        // Trigger flycheck on servers that only run diagnostics on save.
        if client.wants_did_save() {
            let baseline_ticks = {
                let s = Arc::clone(&server);
                tokio::task::spawn_blocking(move || {
                    s.sample_tree().map_or(0, |snap| snap.cumulative_ticks)
                })
                .await
                .unwrap_or(0)
            };

            if let Err(e) = client.did_save(uri).await {
                warn!(
                    server = %server_name,
                    "didSave failed, skipping server: {e}",
                );
                return empty;
            }

            let post_detector = IdleDetector::after_activity(baseline_ticks);
            let post_result = await_idle(&server, post_detector, cancel).await;
            debug!(
                server = %server_name,
                "post-didSave idle result: {post_result:?}",
            );

            if post_result == SettleResult::RootDied
                || matches!(
                    client.lifecycle(),
                    ServerLifecycle::Failed | ServerLifecycle::Dead
                )
            {
                return empty;
            }
        }

        // Retrieve diagnostics: push cache first, pull fallback
        let diagnostics = {
            let cached = client.get_diagnostics(uri);
            if !cached.is_empty() {
                cached
            } else if client.supports_pull_diagnostics() {
                match client.pull_diagnostics(uri).await {
                    Ok(diags) => diags,
                    Err(e) => {
                        client.server().downgrade_pull_diagnostics();
                        debug!("pull diagnostics failed, downgraded to push-only: {e}");
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            }
        };

        // Extract filter context from this specific server
        let server_command = client.server_command().to_string();
        let server_version = client.server_version().map(str::to_string);
        let lang_id = client.language().to_string();

        // Collect quick-fix code actions for each diagnostic
        let fixes = if !diagnostics.is_empty()
            && client
                .capabilities()
                .get("codeActionProvider")
                .is_some_and(|v| !v.is_null())
        {
            collect_quick_fixes(&client, uri, &diagnostics).await
        } else {
            Vec::new()
        };

        // Drop client lock before config access
        drop(client);

        // Per-server min_severity: look up by this server's config name
        let min_severity = {
            let config = self.client_manager.config();
            config
                .server
                .get(&server_name)
                .and_then(|sd| sd.min_severity.as_deref())
                .and_then(crate::filter::parse_severity)
        };

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
        let formatted = if diagnostics.is_empty() {
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

        ServerDiagnostics { formatted, count }
    }

    /// Processes multiple file changes and returns a combined diagnostics
    /// summary.
    ///
    /// Runs the full pipeline for each file and categorizes results:
    /// - Files with diagnostics are listed with their formatted output.
    /// - Clean files (server ran, no issues) are grouped on one line.
    /// - Uncovered files (no language server) are grouped as N/A.
    ///
    /// File paths are displayed relative to their owning workspace root.
    pub async fn process_files(&self, files: &[&str], entry_id: i64) -> String {
        use std::fmt::Write;

        // Notify servers about filesystem changes once before the batch.
        self.client_manager.notify_file_changes().await;

        let roots = self.client_manager.roots();
        let rel = |file: &str| -> String {
            let path = std::path::Path::new(file);
            roots
                .iter()
                .filter_map(|r| path.strip_prefix(r).ok())
                .min_by_key(|rel| rel.as_os_str().len())
                .map_or_else(|| file.to_string(), |r| r.to_string_lossy().to_string())
        };

        let mut diagnostics_output = String::new();
        let mut clean: Vec<String> = Vec::new();
        let mut uncovered: Vec<String> = Vec::new();

        for &file in files {
            let Ok(result) = self.process_file(file, entry_id).await else {
                uncovered.push(rel(file));
                continue;
            };

            match result.content.as_str() {
                "[clean]" | "" => clean.push(rel(file)),
                "[no language server]" => uncovered.push(rel(file)),
                _ => {
                    _ = writeln!(diagnostics_output, "{}:", rel(file));
                    for line in result.content.lines() {
                        _ = writeln!(diagnostics_output, "\t{line}");
                    }
                }
            }
        }

        let mut output = String::new();
        if !diagnostics_output.is_empty() {
            output.push_str(&diagnostics_output);
        }
        if !clean.is_empty() {
            _ = writeln!(output, "{}: clean", clean.join(", "));
        }
        if !uncovered.is_empty() {
            _ = writeln!(output, "{}: N/A", uncovered.join(", "));
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
/// Messages are passed through the provided [`crate::filter::DiagnosticFilter`] for noise
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

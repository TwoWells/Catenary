// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Diagnostics pipeline for PostToolUse hook requests.
//!
//! Handles file-change notifications: path resolution, LSP client lookup,
//! document open/change, settle, diagnostics retrieval (push cache first,
//! pull fallback), severity filtering, noise filtering, quick-fix
//! collection, and compact formatting.

use super::path_security::PathValidator;
use super::tool_server::ToolServer;
use crate::lsp::lang::path_to_uri;
use crate::lsp::settle::{SettleResult, settle};
use crate::lsp::state::ServerLifecycle;
use crate::lsp::{LspClient, LspClientManager};
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::debug;

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

    /// Processes a file change and returns diagnostics.
    ///
    /// Pipeline: lifecycle check → probe → didOpen → didSave → settle →
    /// retrieve (push cache first, `[clean]` semantics) → format → didClose.
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
        reason = "Pipeline steps are sequential and cannot be split"
    )]
    pub async fn process_file(&self, file_path: &str, entry_id: i64) -> Result<DiagnosticsResult> {
        let path = resolve_path(file_path)?;

        // Gate on workspace roots: if the LSP server doesn't know about this
        // file's directory, asking for diagnostics is a wasted round-trip.
        let canonical = self.path_validator.read().await.validate_read(&path)?;

        // Try to get the LSP client for this file's language
        let Ok(client_mutex) = self.client_manager.get_client(&canonical).await else {
            return Ok(DiagnosticsResult {
                content: "[no language server]".into(),
                count: 0,
            });
        };

        let mut client = client_mutex.lock().await;
        let lang_id = client.language().to_string();

        // Thread parent_id so LSP requests are correlated with this hook
        client.set_parent_id(Some(entry_id));

        // Check lifecycle before opening the document
        match client.lifecycle() {
            ServerLifecycle::Failed | ServerLifecycle::Dead => {
                client.set_parent_id(None);
                return Ok(DiagnosticsResult {
                    content: "[no language server]".into(),
                    count: 0,
                });
            }
            _ => {}
        }

        let uri = path_to_uri(&canonical);
        let text = tokio::fs::read_to_string(&canonical).await?;

        let mut doc_manager = self.client_manager.doc_manager().lock().await;
        let (first_open, version) = doc_manager.open(&uri);
        drop(doc_manager);

        // From here, must close document on all paths
        let result = self
            .process_file_inner(&client, &uri, &lang_id, first_open, version, &text)
            .await;

        // Always close the document
        let mut doc_manager = self.client_manager.doc_manager().lock().await;
        if doc_manager.close(&uri) {
            let _ = client.did_close(&uri).await;
        }
        drop(doc_manager);

        client.set_parent_id(None);
        drop(client);

        result
    }

    /// Inner pipeline after document open — extracted to ensure the outer
    /// function always runs the close path.
    #[allow(
        clippy::too_many_lines,
        reason = "Pipeline steps are sequential and cannot be split"
    )]
    async fn process_file_inner(
        &self,
        client: &LspClient,
        uri: &str,
        lang_id: &str,
        first_open: bool,
        version: i32,
        text: &str,
    ) -> Result<DiagnosticsResult> {
        // Snapshot diagnostics generation before sending change
        let server = client.server().clone();
        let gen_before = diagnostics_generation(&server, uri);

        // Send didOpen or didChange
        if first_open {
            client.did_open(uri, lang_id, version, text).await?;
        } else {
            client.did_change(uri, version, text).await?;
        }

        // Health probe: verify the server can respond before settling
        if client.lifecycle() == ServerLifecycle::Probing && !client.run_health_probe(uri).await {
            return Ok(DiagnosticsResult {
                content: "[no language server]".into(),
                count: 0,
            });
        }

        // Trigger flycheck on servers that only run diagnostics on save
        if client.wants_did_save() {
            client.did_save(uri).await?;
        }

        // Settle: wait for the server process tree to go quiet.
        // Skip if the push cache already has data (fast server processed
        // the stimulus before we started polling).
        let settle_result = if client.get_diagnostics(uri).is_empty() {
            let cancel = CancellationToken::new();
            let result = settle(&server, cancel).await;
            debug!("settle result: {result:?}");
            Some(result)
        } else {
            debug!("settle skipped: push cache already populated");
            None
        };

        // Check lifecycle after settle — server may have died during settle
        if matches!(settle_result, Some(SettleResult::RootDied))
            || matches!(
                client.lifecycle(),
                ServerLifecycle::Failed | ServerLifecycle::Dead
            )
        {
            return Ok(DiagnosticsResult {
                content: "[no language server]".into(),
                count: 0,
            });
        }

        // Post-settle: wait for diagnostics if they haven't arrived yet.
        // Two cases:
        // 1. Push cache empty — server publishes asynchronously (delayed).
        // 2. Push cache has stale version — concurrent consumer's diagnostics
        //    arrived but ours haven't yet (version mismatch).
        if !diagnostics_current(&server, uri, gen_before, version) {
            debug!("waiting for diagnostics after settle");
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if diagnostics_current(&server, uri, gen_before, version) {
                    debug!("diagnostics arrived");
                    break;
                }
                tokio::select! {
                    () = server.diagnostics_notify.notified() => {}
                    () = tokio::time::sleep_until(deadline) => {
                        debug!("no current diagnostics within 10s post-settle");
                        break;
                    }
                }
            }
        }

        // Retrieve diagnostics: push cache first, pull fallback
        let diagnostics = {
            let cached = client.get_diagnostics(uri);
            if !cached.is_empty() {
                cached
            } else if client.supports_pull_diagnostics() {
                client.pull_diagnostics(uri).await.unwrap_or_default()
            } else {
                // Healthy server settled with nothing to report
                Vec::new()
            }
        };

        // Extract filter context
        let server_command = client.server_command().to_string();
        let server_version = client.server_version().map(str::to_string);

        // Collect quick-fix code actions for each diagnostic
        let fixes = if !diagnostics.is_empty()
            && client
                .capabilities()
                .get("codeActionProvider")
                .is_some_and(|v| !v.is_null())
        {
            collect_quick_fixes(client, uri, &diagnostics).await
        } else {
            Vec::new()
        };

        // Apply severity threshold from config
        let min_severity_str = self
            .client_manager
            .config()
            .resolve_language(lang_id)
            .and_then(|(_, lc)| lc.min_severity);
        let min_severity = min_severity_str
            .as_deref()
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
                lang_id,
            )
        };

        Ok(DiagnosticsResult { content, count })
    }

    /// Processes multiple file changes and returns a combined diagnostics string.
    ///
    /// Runs the full pipeline for each file (document sync, settle, severity
    /// filtering, noise filtering, quick-fixes). Files with `[clean]` or
    /// `[no language server]` results are omitted. Errors are best-effort
    /// skipped.
    pub async fn process_files(&self, files: &[&str], entry_id: i64) -> String {
        use std::fmt::Write;

        let mut output = String::new();

        for &file in files {
            let Ok(result) = self.process_file(file, entry_id).await else {
                continue;
            };

            if result.content.is_empty()
                || result.content == "[clean]"
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

/// Returns the current diagnostics generation for a URI.
fn diagnostics_generation(server: &crate::lsp::LspServer, uri: &str) -> u64 {
    server
        .diagnostics_generation
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(uri)
        .copied()
        .unwrap_or(0)
}

/// Checks whether diagnostics in the push cache are current.
///
/// Diagnostics are current if:
/// - The generation advanced (new diagnostics arrived since stimulus), AND
/// - The cached version matches or exceeds the version we sent (for
///   servers that include version in `publishDiagnostics`).
///
/// For servers that don't publish version, the generation check alone
/// is sufficient.
fn diagnostics_current(
    server: &crate::lsp::LspServer,
    uri: &str,
    gen_before: u64,
    sent_version: i32,
) -> bool {
    use std::sync::atomic::Ordering;

    // Generation must have advanced
    if diagnostics_generation(server, uri) <= gen_before {
        return false;
    }

    // If the server publishes version, verify the cached version matches
    if server.publishes_version.load(Ordering::SeqCst) {
        let cache = server
            .diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((Some(cached_version), _)) = cache.get(uri) {
            return *cached_version >= sent_version;
        }
    }

    true
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

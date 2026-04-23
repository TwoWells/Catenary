// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Diagnostics pipeline for PostToolUse hook requests.
//!
//! Handles file-change notifications: path resolution, LSP client lookup,
//! document open/change, idle detection, diagnostics retrieval (push cache
//! first, pull fallback), severity filtering, noise filtering, quick-fix
//! collection, and compact formatting.

use super::filesystem_manager::FilesystemManager;
use super::path_security::PathValidator;
use super::tool_server::ToolServer;
use crate::lsp::settle::{IdleDetector, SettleResult, await_idle};
use crate::lsp::state::ServerLifecycle;
use crate::lsp::{LspClient, LspClientManager};
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::collections::BTreeMap;
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
    fs: Arc<FilesystemManager>,
}

impl DiagnosticsServer {
    /// Creates a new `DiagnosticsServer`.
    pub const fn new(
        client_manager: Arc<LspClientManager>,
        path_validator: Arc<RwLock<PathValidator>>,
        fs: Arc<FilesystemManager>,
    ) -> Self {
        Self {
            client_manager,
            path_validator,
            fs,
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
        let mut client = client_mutex.lock().await;
        client.close_tracked_document(&uri).await;
        client.set_parent_id(None);
        drop(client);

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

    /// Processes multiple file changes with a batched lifecycle so
    /// servers see all modified files simultaneously.
    ///
    /// Pipeline: notify file changes → resolve + canonicalize →
    /// group by server → per server (open all → settle → health
    /// probe → didSave all → settle → retrieve per file → close
    /// all) → format → `mark_current`.
    ///
    /// Cross-file diagnostics (e.g., a renamed type that breaks
    /// importers) are correct because every server sees the complete
    /// final state before producing diagnostics.
    #[allow(
        clippy::too_many_lines,
        reason = "Batch pipeline steps are sequential and cannot be split"
    )]
    #[allow(
        clippy::type_complexity,
        reason = "Server grouping map is local and self-documenting"
    )]
    pub async fn process_files_batched(&self, files: &[PathBuf], entry_id: i64) -> String {
        use std::fmt::Write;

        if files.is_empty() {
            return "[clean]\n".to_string();
        }

        // Notify servers about filesystem changes once before the batch.
        self.client_manager.notify_file_changes().await;

        // ── Phase 1: resolve + canonicalize ────────────────────────
        let mut canonical_paths: Vec<PathBuf> = Vec::new();
        let mut uncovered: Vec<String> = Vec::new();

        // Server → list of canonical paths.
        // Keyed by server name for stable (alphabetical) iteration order.
        let mut server_groups: BTreeMap<String, (Arc<Mutex<LspClient>>, Vec<PathBuf>)> =
            BTreeMap::new();

        let validator = self.path_validator.read().await;
        for file in files {
            let file_str = file.to_string_lossy();

            // Resolve to absolute if needed (drain_all_and_clear
            // already returns absolute paths, but be defensive).
            let Ok(path) = resolve_path(&file_str) else {
                uncovered.push(self.display_rel(&file_str));
                continue;
            };

            let Ok(canonical) = validator.validate_read(&path) else {
                uncovered.push(self.display_rel(&file_str));
                continue;
            };

            let clients = self.client_manager.diagnostic_servers(&canonical).await;
            if clients.is_empty() {
                uncovered.push(self.display_rel(&canonical.to_string_lossy()));
                continue;
            }

            canonical_paths.push(canonical.clone());

            for client_mutex in &clients {
                let name = client_mutex.lock().await.server_name().to_string();
                server_groups
                    .entry(name)
                    .or_insert_with(|| (Arc::clone(client_mutex), Vec::new()))
                    .1
                    .push(canonical.clone());
            }
        }
        drop(validator);

        // ── Phase 2: per-server batch lifecycle ────────────────────
        // Collect per-file diagnostics across all servers.
        // Key: canonical path string → (display path, Vec<ServerDiagnostics>).
        let mut file_results: BTreeMap<String, (String, Vec<ServerDiagnostics>)> = BTreeMap::new();

        for (client_mutex, paths) in server_groups.values() {
            self.run_server_batch(client_mutex, paths, entry_id, &mut file_results)
                .await;
        }

        // ── Phase 3: format output ────────────────────────────────
        let mut diagnostics_output = String::new();
        let mut clean: Vec<String> = Vec::new();

        for (display, segments) in file_results.values() {
            let total_count: usize = segments.iter().map(|s| s.count).sum();
            if total_count == 0 {
                clean.push(display.clone());
            } else {
                _ = writeln!(diagnostics_output, "{display}:");
                for seg in segments {
                    if !seg.formatted.is_empty() {
                        for line in seg.formatted.lines() {
                            _ = writeln!(diagnostics_output, "\t{line}");
                        }
                    }
                }
            }
        }

        // Files that were validated but had no server results (all
        // servers died during pipeline) — treat as clean.
        for cp in &canonical_paths {
            let key = cp.to_string_lossy().to_string();
            if !file_results.contains_key(&key) {
                clean.push(self.display_rel(&key));
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

        // ── Phase 4: mark_current ─────────────────────────────────
        self.fs.mark_current(&canonical_paths);

        output
    }

    /// Runs the batched diagnostics lifecycle on a single server.
    ///
    /// Opens all files, settles, runs health probe if needed,
    /// sends didSave, settles again, retrieves diagnostics per file,
    /// and closes all files.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "Client lock held across pipeline for exclusive access"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "Pipeline steps are sequential and cannot be split"
    )]
    async fn run_server_batch(
        &self,
        client_mutex: &Arc<Mutex<LspClient>>,
        paths: &[PathBuf],
        entry_id: i64,
        file_results: &mut BTreeMap<String, (String, Vec<ServerDiagnostics>)>,
    ) {
        // ── Open all files ─────────────────────────────────────────
        let mut opened_uris: Vec<(PathBuf, String)> = Vec::new();

        for path in paths {
            match self
                .client_manager
                .open_document_on(path, client_mutex, Some(entry_id))
                .await
            {
                Ok(uri) => opened_uris.push((path.clone(), uri)),
                Err(e) => {
                    let name = client_mutex.lock().await.server_name().to_string();
                    warn!(
                        server = %name,
                        path = %path.display(),
                        "batch open failed, skipping file: {e}",
                    );
                }
            }
        }

        if opened_uris.is_empty() {
            return;
        }

        // ── Settle after all opens ─────────────────────────────────
        let client = client_mutex.lock().await;

        if matches!(
            client.lifecycle(),
            ServerLifecycle::Failed | ServerLifecycle::Dead
        ) {
            // Close whatever we opened and bail.
            drop(client);
            self.close_all(client_mutex, &opened_uris).await;
            return;
        }

        let server = client.server().clone();
        let server_name = client.server_name().to_string();
        let cancel = CancellationToken::new();

        let pre_detector = IdleDetector::unconditional();
        let pre_result = await_idle(&server, pre_detector, cancel.clone()).await;
        debug!(
            server = %server_name,
            "batch idle result: {pre_result:?}",
        );

        if pre_result == SettleResult::RootDied
            || matches!(
                client.lifecycle(),
                ServerLifecycle::Failed | ServerLifecycle::Dead
            )
        {
            drop(client);
            self.close_all(client_mutex, &opened_uris).await;
            return;
        }

        // ── Health probe ───────────────────────────────────────────
        if client.lifecycle() == ServerLifecycle::Probing
            && !client.run_health_probe(&opened_uris[0].1).await
        {
            drop(client);
            self.close_all(client_mutex, &opened_uris).await;
            return;
        }

        // ── didSave all ────────────────────────────────────────────
        if client.wants_did_save() {
            let baseline_ticks = {
                let s = Arc::clone(&server);
                tokio::task::spawn_blocking(move || {
                    s.sample_tree().map_or(0, |snap| snap.cumulative_ticks)
                })
                .await
                .unwrap_or(0)
            };

            let mut save_failed = false;
            for (_, uri) in &opened_uris {
                if let Err(e) = client.did_save(uri).await {
                    warn!(
                        server = %server_name,
                        "batch didSave failed: {e}",
                    );
                    save_failed = true;
                    break;
                }
            }

            if save_failed {
                drop(client);
                self.close_all(client_mutex, &opened_uris).await;
                return;
            }

            let post_detector = IdleDetector::after_activity(baseline_ticks);
            let post_result = await_idle(&server, post_detector, cancel).await;
            debug!(
                server = %server_name,
                "batch post-didSave idle result: {post_result:?}",
            );

            if post_result == SettleResult::RootDied
                || matches!(
                    client.lifecycle(),
                    ServerLifecycle::Failed | ServerLifecycle::Dead
                )
            {
                drop(client);
                self.close_all(client_mutex, &opened_uris).await;
                return;
            }
        }

        // ── Retrieve diagnostics per file ──────────────────────────
        let server_command = client.server_command().to_string();
        let server_version = client.server_version().map(str::to_string);
        let lang_id = client.language().to_string();
        let has_code_actions = client
            .capabilities()
            .get("codeActionProvider")
            .is_some_and(|v| !v.is_null());

        for (path, uri) in &opened_uris {
            let diagnostics = {
                let cached = client.get_diagnostics(uri);
                if !cached.is_empty() {
                    cached
                } else if client.supports_pull_diagnostics() {
                    match client.pull_diagnostics(uri).await {
                        Ok(diags) => diags,
                        Err(e) => {
                            client.server().downgrade_pull_diagnostics();
                            debug!("pull diagnostics failed, downgraded: {e}");
                            Vec::new()
                        }
                    }
                } else {
                    Vec::new()
                }
            };

            let fixes = if !diagnostics.is_empty() && has_code_actions {
                collect_quick_fixes(&client, uri, &diagnostics).await
            } else {
                Vec::new()
            };

            // Apply per-server min_severity filter
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

            let key = path.to_string_lossy().to_string();
            let display = self.display_rel(&key);
            file_results
                .entry(key)
                .or_insert_with(|| (display, Vec::new()))
                .1
                .push(ServerDiagnostics { formatted, count });
        }

        // ── Close all ──────────────────────────────────────────────
        drop(client);
        self.close_all(client_mutex, &opened_uris).await;
    }

    /// Closes all opened documents on a server and clears `parent_id`.
    async fn close_all(
        &self,
        client_mutex: &Arc<Mutex<LspClient>>,
        opened_uris: &[(PathBuf, String)],
    ) {
        let mut client = client_mutex.lock().await;
        for (_, uri) in opened_uris {
            client.close_tracked_document(uri).await;
        }
        client.set_parent_id(None);
    }

    /// Makes a path relative to the owning workspace root, for display.
    fn display_rel(&self, file: &str) -> String {
        let path = std::path::Path::new(file);
        self.fs.resolve_root(path).map_or_else(
            || file.to_string(),
            |root| {
                path.strip_prefix(&root).map_or_else(
                    |_| file.to_string(),
                    |rel| rel.to_string_lossy().to_string(),
                )
            },
        )
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

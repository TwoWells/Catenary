// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Bridge handler that maps MCP tool calls to LSP requests.

use anyhow::{Result, anyhow};
use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{Searcher, Sink, SinkMatch};
use ignore::WalkBuilder;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use super::symbols::{
    self, SymbolInfo, extract_locations, extract_symbol_infos, format_symbol_kind,
};

use crate::lsp::{ClientManager, LspClient};
use crate::mcp::{CallToolResult, Tool, ToolContent, ToolHandler};
use crate::session::{EventBroadcaster, EventKind};

/// Maximum unique LSP symbols for hover display in output. Above this
/// threshold, hover content is omitted but structural enrichment (references,
/// callers, implementations, subtypes) is always included. Also caps the
/// bootstrap discovery loop.
const GREP_HOVER_THRESHOLD: usize = 10;

use super::diagnostics_server::DiagnosticsServer;
use super::replace::ReplaceServer;
use super::tool_server::ToolServer;
use super::{DocumentManager, DocumentNotification};

/// Result of a server health check against touched language servers.
struct ServerHealth {
    /// Languages with dead servers.
    dead: Vec<String>,
    /// One-time batched notification for state transitions (offline/recovery).
    notification: Option<String>,
}

/// Input for grep tool.
#[derive(Debug, Deserialize)]
pub struct GrepInput {
    /// Search pattern (supports `|` for alternation, passed to ripgrep).
    pub pattern: String,
}

/// Bridge handler that implements MCP `ToolHandler` trait.
/// Handles MCP tool calls by routing them to the appropriate LSP server.
pub struct LspBridgeHandler {
    pub(super) client_manager: Arc<ClientManager>,
    pub(super) doc_manager: Arc<Mutex<DocumentManager>>,
    pub(super) runtime: Handle,
    pub(super) broadcaster: EventBroadcaster,
    /// Languages whose servers have been reported offline to the agent.
    /// Used for one-time notification: offline is reported once, recovery once.
    notified_offline: std::sync::Mutex<HashSet<String>>,
    /// Whether to capture full tool output in `ToolResult` events.
    capture_tool_output: bool,
    /// Batch replacement tool with snapshots and diagnostics.
    replace: ReplaceServer,
}

impl LspBridgeHandler {
    /// Creates a new `LspBridgeHandler`.
    pub fn new(
        client_manager: Arc<ClientManager>,
        doc_manager: Arc<Mutex<DocumentManager>>,
        runtime: Handle,
        broadcaster: EventBroadcaster,
        capture_tool_output: bool,
        diagnostics: Arc<DiagnosticsServer>,
        session_id: Option<String>,
    ) -> Self {
        let replace = ReplaceServer::new(
            client_manager.clone(),
            doc_manager.clone(),
            diagnostics,
            runtime.clone(),
            session_id,
        );
        Self {
            client_manager,
            doc_manager,
            runtime,
            broadcaster,
            notified_offline: std::sync::Mutex::new(HashSet::new()),
            capture_tool_output,
            replace,
        }
    }
    /// Gets the appropriate LSP client for the given file path.
    pub(super) async fn get_client_for_path(&self, path: &Path) -> Result<Arc<Mutex<LspClient>>> {
        let lang_id = {
            let doc_manager = self.doc_manager.lock().await;
            doc_manager.language_id_for_path(path).to_string()
        };

        self.client_manager
            .get_client_for_path(path, &lang_id)
            .await
    }

    /// Waits for the server handling the given path to be ready.
    ///
    /// Dead servers are non-fatal — the wait completes and the caller
    /// uses [`check_server_health`] to detect the state.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "Client lock held across wait_ready call"
    )]
    async fn wait_for_server_ready(&self, path: &Path) {
        let Ok(client_mutex) = self.get_client_for_path(path).await else {
            return; // No LSP server configured for this language
        };

        let client = client_mutex.lock().await;
        let lang = client.language().to_string();
        let is_ready = client.wait_ready().await;
        drop(client);

        if !is_ready {
            warn!("[{lang}] server died \u{2014} tool will run in degraded mode");
        }
    }

    /// Waits for all active LSP servers to be ready.
    ///
    /// Dead servers are non-fatal — the wait completes for each server
    /// and the caller uses [`check_server_health`] to detect state.
    /// Used for symbol-only queries that don't target a specific file.
    async fn wait_for_all_servers_ready(&self) {
        let clients = self.client_manager.active_clients().await;

        for (lang, client_mutex) in clients {
            if !client_mutex.lock().await.wait_ready().await {
                warn!("[{lang}] server died \u{2014} tool will run in degraded mode");
            }
        }
    }

    /// Checks server health for the given languages and generates one-time
    /// state-transition notifications.
    ///
    /// Queries each server's liveness, partitions into alive/dead, and
    /// compares against `notified_offline` to produce batched notifications:
    /// - Newly dead servers get a single offline message with scope of impact.
    /// - Previously-offline servers that recovered get a single recovery message.
    fn check_server_health(&self, touched_servers: &[String]) -> ServerHealth {
        let mut alive = Vec::new();
        let mut dead = Vec::new();

        // Classify each touched server by readiness (not just process liveness —
        // a stuck server is alive but not ready)
        let clients = self.runtime.block_on(self.client_manager.active_clients());
        for lang in touched_servers {
            let ready = clients.get(lang).is_some_and(|c| {
                self.runtime.block_on(async {
                    let client = c.lock().await;
                    // Lightweight idle probe: if a stuck server has gone idle,
                    // recover it to Ready before checking readiness.
                    client.try_idle_recover();
                    client.is_ready()
                })
            });

            if ready {
                alive.push(lang.clone());
            } else {
                dead.push(lang.clone());
            }
        }

        // Determine state transitions against notified_offline
        let mut notified = self
            .notified_offline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut parts = Vec::new();

        // Recovery: previously unavailable, now alive
        let recovered: Vec<String> = alive
            .iter()
            .filter(|lang| notified.remove(lang.as_str()))
            .cloned()
            .collect();

        if !recovered.is_empty() {
            let langs = recovered.join(", ");
            parts.push(format!(
                "Language server{} back online: {langs} \u{2014} \
                 diagnostics and language server enrichment re-enabled for \
                 {langs} files.",
                if recovered.len() == 1 { "" } else { "s" },
            ));
        }

        // Unavailable: newly dead or stuck, not yet reported
        let newly_dead: Vec<String> = dead
            .iter()
            .filter(|lang| notified.insert((*lang).clone()))
            .cloned()
            .collect();

        if !newly_dead.is_empty() {
            let langs = newly_dead.join(", ");
            parts.push(format!(
                "Language server{} unavailable: {langs} \u{2014} \
                 diagnostics unavailable for {langs} files. \
                 grep and glob still work but without \
                 language server enrichment.",
                if newly_dead.len() == 1 { "" } else { "s" },
            ));
        }

        let notification = if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        };

        ServerHealth { dead, notification }
    }

    /// Extract file path from arguments if present.
    fn extract_file_path(arguments: Option<&serde_json::Value>) -> Option<PathBuf> {
        arguments
            .and_then(|v| v.get("file"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
    }

    /// Returns the language key for a file path, matching the key used in
    /// `active_clients()`. This may differ from the LSP language ID for
    /// custom/test languages where the config key is the file extension.
    async fn language_for_path(&self, path: &Path) -> Option<String> {
        let lang_id = {
            let doc_manager = self.doc_manager.lock().await;
            doc_manager.language_id_for_path(path).to_string()
        };
        let client_mutex = self
            .client_manager
            .get_client_for_path(path, &lang_id)
            .await
            .ok()?;
        Some(client_mutex.lock().await.language().to_string())
    }

    /// Ensures a document is open and synced with the LSP server.
    pub(super) async fn ensure_document_open(
        &self,
        path: &Path,
    ) -> Result<(String, Arc<Mutex<LspClient>>)> {
        let client_mutex = self.get_client_for_path(path).await?;
        let mut doc_manager = self.doc_manager.lock().await;
        let client = client_mutex.lock().await;

        // Check if LSP is still alive
        if !client.is_alive() {
            return Err(anyhow!(
                "[{}] server is no longer running",
                client.language()
            ));
        }

        let uri = doc_manager.uri_for_path(path)?;

        if let Some(notification) = doc_manager.ensure_open(path).await? {
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

            drop(doc_manager);
            drop(client);
            return Ok((uri, client_mutex.clone()));
        }

        drop(doc_manager);
        drop(client);
        Ok((uri, client_mutex.clone()))
    }

    /// Resolves a file path, converting relative paths to absolute using the current working directory.
    pub(super) fn resolve_path(file: &str) -> Result<PathBuf> {
        let path = PathBuf::from(file);
        if path.is_absolute() {
            Ok(path)
        } else {
            let cwd = std::env::current_dir()
                .map_err(|e| anyhow!("Failed to get current working directory: {e}"))?;
            Ok(cwd.join(path))
        }
    }

    /// Grep: ripgrep + `workspace/symbol("")` pipeline with LSP enrichment.
    #[allow(clippy::too_many_lines, reason = "Core grep orchestration")]
    fn handle_grep(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        use std::fmt::Write;

        let input: GrepInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        if input.pattern.is_empty() {
            return Err(anyhow!("pattern must be non-empty"));
        }

        let re = Regex::new(&format!("(?i){}", &input.pattern))
            .map_err(|e| anyhow!("Invalid regex pattern: {e}"))?;

        debug!("Grep request: pattern={}", input.pattern);

        let roots = self.runtime.block_on(self.client_manager.roots());

        // 1. Ripgrep: get matched strings + file/line heatmap in one pass
        let rg = Self::ripgrep_matches(&input.pattern, &roots)?;

        // 2. Symbol universe: workspace/symbol("") + regex filter, with rg fallback
        let symbols = self.runtime.block_on(async {
            let clients = self.client_manager.active_clients().await;

            // Try workspace/symbol("") first — returns the full symbol index
            let mut all_symbols: Vec<SymbolInfo> = self.fetch_symbol_universe(&clients).await;

            // Fallback: if symbol("") returned nothing, use rg matched strings
            if all_symbols.is_empty() && !rg.matched_strings.is_empty() {
                all_symbols = self
                    .fetch_symbols_by_queries(&rg.matched_strings, &clients)
                    .await;
            }

            // Regex filter against the user's pattern
            all_symbols.retain(|s| re.is_match(&s.name));

            // Dedupe by (name, file_path, line)
            let mut seen: HashSet<(String, String, u32)> = HashSet::new();
            all_symbols.retain(|s| seen.insert((s.name.clone(), s.file_path.clone(), s.line)));

            all_symbols
        });

        let show_hover = symbols.len() <= GREP_HOVER_THRESHOLD;

        // 3. Group symbols by name (preserving order of first occurrence)
        let mut name_order: Vec<String> = Vec::new();
        let mut by_name: BTreeMap<String, Vec<&SymbolInfo>> = BTreeMap::new();
        for sym in &symbols {
            if !by_name.contains_key(&sym.name) {
                name_order.push(sym.name.clone());
            }
            by_name.entry(sym.name.clone()).or_default().push(sym);
        }

        // 4. Always enrich: references, callers, implementations, subtypes
        let mut enrichments: HashMap<(String, u32), SymbolEnrichment> = HashMap::new();
        let mut all_ref_lines: HashMap<String, HashSet<u32>> = HashMap::new();

        for sym in &symbols {
            let enrichment = self.enrich_symbol(sym);
            for (file, lines) in &enrichment.ref_lines {
                all_ref_lines.entry(file.clone()).or_default().extend(lines);
            }
            let key = (sym.file_path.clone(), sym.line);
            enrichments.insert(key, enrichment);
        }

        // 4b. Rg-bootstrapped enrichment: enrich unaccounted rg hits via hover
        let br = self.bootstrap_from_rg(&rg, &by_name, &all_ref_lines);
        enrichments.extend(br.enrichments);
        for (file, lines) in br.ref_lines {
            all_ref_lines.entry(file).or_default().extend(lines);
        }
        for n in &br.name_order {
            if !name_order.contains(n) {
                name_order.push(n.clone());
            }
        }
        let bootstrapped = br.symbols;
        for sym in &bootstrapped {
            by_name.entry(sym.name.clone()).or_default().push(sym);
        }

        // 5. Assign rg lines to symbol names; leftover lines are non-code hits
        let rg_by_name = assign_rg_lines_to_symbols(&by_name, &all_ref_lines, &rg);

        // 6. Build unified name_order: LSP symbol names + rg-only matched strings
        // Add rg-only headings (matched strings that don't correspond to any LSP symbol)
        for heading in rg_by_name.keys() {
            if !heading.is_empty() && !name_order.contains(heading) {
                name_order.push(heading.clone());
            }
        }

        if name_order.is_empty() && rg.file_lines.is_empty() {
            return Ok(CallToolResult::text("No results found".to_string()));
        }

        let mut output = String::new();

        // Single loop over all headings (LSP symbols and rg-only)
        for name in &name_order {
            if !output.is_empty() {
                output.push('\n');
            }
            let _ = writeln!(output, "# {name}");

            // Non-code rg hits for this heading, with hover-resolved context
            if let Some(lines) = rg_by_name.get(name.as_str())
                && !lines.is_empty()
            {
                let _ = writeln!(output);
                for (file, file_lines) in lines {
                    let path = display_path(file, &roots);
                    let _ = writeln!(output, "{path} {}", format_line_ranges(file_lines));
                }
            }

            // Definition sub-headings (only for LSP symbols)
            if let Some(defs) = by_name.get(name) {
                for sym in defs {
                    let kind = format_symbol_kind(sym.kind);
                    let path = display_path(&sym.file_path, &roots);
                    let line = sym.line + 1;
                    let _ = writeln!(output, "\n## [{kind}] {path}:{line}");

                    let key = (sym.file_path.clone(), sym.line);
                    if let Some(enrichment) = enrichments.get(&key) {
                        // Hover only when symbol count is within threshold
                        if show_hover && let Some(hover) = &enrichment.hover {
                            output.push('\n');
                            for line in hover.lines() {
                                let _ = writeln!(output, "> {line}");
                            }
                        }

                        // Structural enrichment always shown
                        let mut labeled_lines: HashSet<(String, u32)> = HashSet::new();

                        if !enrichment.incoming_calls.is_empty() {
                            let _ = writeln!(output, "\n### Callers\n");
                            for (name, file, line) in &enrichment.incoming_calls {
                                let path = display_path(file, &roots);
                                let _ = writeln!(output, "{name}  {path}:{line}");
                                labeled_lines.insert((file.clone(), *line));
                            }
                        }

                        if !enrichment.implementations.is_empty() {
                            let _ = writeln!(output, "\n### Implementations\n");
                            for (file, line) in &enrichment.implementations {
                                let path = display_path(file, &roots);
                                let _ = writeln!(output, "{path}:{line}");
                                labeled_lines.insert((file.clone(), *line));
                            }
                        }

                        if !enrichment.subtypes.is_empty() {
                            let _ = writeln!(output, "\n### Subtypes\n");
                            for (name, file, line) in &enrichment.subtypes {
                                let path = display_path(file, &roots);
                                let _ = writeln!(output, "{name}  {path}:{line}");
                                labeled_lines.insert((file.clone(), *line));
                            }
                        }

                        // References (excluding definition line and labeled lines)
                        let ref_output = format_symbol_references(
                            enrichment,
                            &sym.file_path,
                            sym.line,
                            &roots,
                            &labeled_lines,
                        );
                        if !ref_output.is_empty() {
                            let _ = writeln!(output, "\n### References\n\n{ref_output}");
                        }
                    }
                }
            }
        }

        // Trim trailing whitespace
        let trimmed_len = output.trim_end().len();
        output.truncate(trimmed_len);

        if output.is_empty() {
            return Ok(CallToolResult::text("No results found".to_string()));
        }

        Ok(CallToolResult::text(output))
    }

    /// Fetches the full symbol universe via `workspace/symbol("")` from all servers.
    /// Resolves URI-only symbols when the server supports `workspaceSymbol/resolve`.
    async fn fetch_symbol_universe(
        &self,
        clients: &HashMap<String, Arc<Mutex<LspClient>>>,
    ) -> Vec<SymbolInfo> {
        let mut all_symbols: Vec<SymbolInfo> = Vec::new();

        for client_mutex in clients.values() {
            let client = client_mutex.lock().await;
            let supports_resolve = client.supports_workspace_symbol_resolve();

            let Ok(response) = client.workspace_symbols("").await else {
                continue;
            };

            // Extract symbols that have full location info
            all_symbols.extend(extract_symbol_infos(&response));

            // Resolve URI-only symbols when server supports it
            if supports_resolve && let Some(arr) = response.as_array() {
                for item in arr {
                    let has_uri = item.get("location").and_then(|l| l.get("uri")).is_some();
                    let has_range = item.get("location").and_then(|l| l.get("range")).is_some();
                    if has_uri
                        && !has_range
                        && let Ok(resolved) = client.workspace_symbol_resolve(item).await
                    {
                        all_symbols.extend(extract_symbol_infos(&Value::Array(vec![resolved])));
                    }
                }
            }
        }

        all_symbols
    }

    /// Fallback: queries workspace/symbol with each matched string (ticket 08 behavior).
    /// Resolves URI-only symbols via resolve when supported.
    async fn fetch_symbols_by_queries(
        &self,
        queries: &[String],
        clients: &HashMap<String, Arc<Mutex<LspClient>>>,
    ) -> Vec<SymbolInfo> {
        let mut all_symbols: Vec<SymbolInfo> = Vec::new();

        for query in queries {
            for client_mutex in clients.values() {
                let client = client_mutex.lock().await;
                let supports_resolve = client.supports_workspace_symbol_resolve();

                let Ok(response) = client.workspace_symbols(query).await else {
                    continue;
                };

                all_symbols.extend(extract_symbol_infos(&response));

                if supports_resolve && let Some(arr) = response.as_array() {
                    for item in arr {
                        let has_uri = item.get("location").and_then(|l| l.get("uri")).is_some();
                        let has_range = item.get("location").and_then(|l| l.get("range")).is_some();
                        if has_uri
                            && !has_range
                            && let Ok(resolved) = client.workspace_symbol_resolve(item).await
                        {
                            all_symbols.extend(extract_symbol_infos(&Value::Array(vec![resolved])));
                        }
                    }
                }
            }
        }

        all_symbols
    }

    /// Enriches a symbol with hover, references, and kind-specific labels.
    fn enrich_symbol(&self, sym: &SymbolInfo) -> SymbolEnrichment {
        let path = PathBuf::from(&sym.file_path);
        self.enrich_at_position(&path, sym.line, sym.character, sym.kind)
    }

    /// Enriches a position with hover, references, and kind-specific labels.
    #[allow(clippy::too_many_lines, reason = "Sequential LSP calls by kind")]
    fn enrich_at_position(
        &self,
        path: &Path,
        line_0: u32,
        col: u32,
        kind: u32,
    ) -> SymbolEnrichment {
        self.runtime.block_on(async {
            let mut enrichment = SymbolEnrichment::default();

            let Ok((uri_str, client_mutex)) = self.ensure_document_open(path).await else {
                return enrichment;
            };

            let client = client_mutex.lock().await;
            let caps = client.capabilities();

            // Hover — signature + docs
            if has_cap(caps, "hoverProvider")
                && let Ok(hover) = client.hover(&uri_str, line_0, col).await
            {
                enrichment.hover = extract_hover_text_from_value(&hover);
            }

            // References — collect line numbers for dedup
            if has_cap(caps, "referencesProvider")
                && let Ok(refs) = client.references(&uri_str, line_0, col, true).await
            {
                for (file, line, _char) in extract_locations(&refs) {
                    enrichment
                        .ref_lines
                        .entry(file)
                        .or_default()
                        .insert(line + 1);
                }
            }

            // Kind-specific enrichment
            match kind {
                // Functions/methods/constructors → incoming calls
                symbols::SK_FUNCTION | symbols::SK_METHOD | symbols::SK_CONSTRUCTOR => {
                    if has_cap(caps, "callHierarchyProvider")
                        && let Ok(response) =
                            client.prepare_call_hierarchy(&uri_str, line_0, col).await
                        && let Some(items) = response.as_array()
                    {
                        for item in items {
                            if let Ok(calls) = client.incoming_calls(item).await {
                                extract_incoming_calls(&calls, &mut enrichment);
                            }
                        }
                    }
                }

                // Structs/classes/enums → implementations
                symbols::SK_STRUCT | symbols::SK_CLASS | symbols::SK_ENUM => {
                    if has_cap(caps, "implementationProvider")
                        && let Ok(response) = client.implementation(&uri_str, line_0, col).await
                    {
                        for (file, line, _char) in extract_locations(&response) {
                            enrichment.implementations.push((file, line + 1));
                        }
                    }
                }

                // Interfaces/traits → subtypes
                symbols::SK_INTERFACE => {
                    if client.supports_type_hierarchy()
                        && let Ok(response) =
                            client.prepare_type_hierarchy(&uri_str, line_0, col).await
                        && let Some(items) = response.as_array()
                    {
                        for item in items {
                            if let Ok(subs) = client.subtypes(item).await {
                                extract_subtypes(&subs, &mut enrichment);
                            }
                        }
                    }
                }

                _ => {}
            }

            enrichment
        })
    }

    /// Enriches a position with kind inference — tries all kind-specific
    /// enrichments and infers kind from what returns results. Used by the
    /// rg-bootstrapped enrichment path where kind is unknown.
    #[allow(
        clippy::too_many_lines,
        reason = "Sequential LSP calls for kind inference"
    )]
    fn enrich_at_position_infer_kind(
        &self,
        path: &Path,
        line_0: u32,
        col: u32,
    ) -> (u32, Option<String>, SymbolEnrichment) {
        self.runtime.block_on(async {
            let mut enrichment = SymbolEnrichment::default();
            let mut inferred_kind = symbols::SK_VARIABLE;
            let mut resolved_name: Option<String> = None;

            let Ok((uri_str, client_mutex)) = self.ensure_document_open(path).await else {
                return (inferred_kind, resolved_name, enrichment);
            };

            let client = client_mutex.lock().await;
            let caps = client.capabilities();

            // Hover — signature + docs
            if has_cap(caps, "hoverProvider")
                && let Ok(hover) = client.hover(&uri_str, line_0, col).await
            {
                enrichment.hover = extract_hover_text_from_value(&hover);
            }

            // References — collect line numbers for dedup
            if has_cap(caps, "referencesProvider")
                && let Ok(refs) = client.references(&uri_str, line_0, col, true).await
            {
                for (file, line, _char) in extract_locations(&refs) {
                    enrichment
                        .ref_lines
                        .entry(file)
                        .or_default()
                        .insert(line + 1);
                }
            }

            // Try all kind-specific enrichments — infer kind from results

            // Call hierarchy → FUNCTION
            if has_cap(caps, "callHierarchyProvider")
                && let Ok(response) = client.prepare_call_hierarchy(&uri_str, line_0, col).await
                && let Some(items) = response.as_array()
                && !items.is_empty()
            {
                inferred_kind = symbols::SK_FUNCTION;
                // The first item's name is the LSP-canonical symbol name.
                resolved_name = items
                    .first()
                    .and_then(|i| i.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                for item in items {
                    if let Ok(calls) = client.incoming_calls(item).await {
                        extract_incoming_calls(&calls, &mut enrichment);
                    }
                }
            }

            // Implementation → STRUCT (only if not already identified as function)
            if inferred_kind == symbols::SK_VARIABLE
                && has_cap(caps, "implementationProvider")
                && let Ok(response) = client.implementation(&uri_str, line_0, col).await
            {
                let locs = extract_locations(&response);
                if !locs.is_empty() {
                    inferred_kind = symbols::SK_STRUCT;
                    for (file, line, _char) in locs {
                        enrichment.implementations.push((file, line + 1));
                    }
                }
            }

            // Type hierarchy → INTERFACE (only if not already identified)
            if inferred_kind == symbols::SK_VARIABLE
                && client.supports_type_hierarchy()
                && let Ok(response) = client.prepare_type_hierarchy(&uri_str, line_0, col).await
                && let Some(items) = response.as_array()
                && !items.is_empty()
            {
                inferred_kind = symbols::SK_INTERFACE;
                resolved_name = items
                    .first()
                    .and_then(|i| i.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                for item in items {
                    if let Ok(subs) = client.subtypes(item).await {
                        extract_subtypes(&subs, &mut enrichment);
                    }
                }
            }

            (inferred_kind, resolved_name, enrichment)
        })
    }

    /// Bootstraps LSP enrichment from unaccounted rg hit positions.
    ///
    /// After the symbol universe enrichment, some rg hits may not be explained
    /// by any universe symbol (truncation, keyword prefixes). This method
    /// uses `prepareRename` to distinguish symbols from keywords, then
    /// enriches confirmed symbols with full LSP queries.
    ///
    /// Returns owned `SymbolInfo` values (plus enrichments and ref lines).
    /// The caller merges these into `by_name` where both the universe
    /// `symbols` vec and the returned vec are in scope.
    #[allow(clippy::too_many_lines, reason = "Iterative elimination loop")]
    fn bootstrap_from_rg(
        &self,
        rg: &RipgrepMatches,
        by_name: &BTreeMap<String, Vec<&SymbolInfo>>,
        all_ref_lines: &HashMap<String, HashSet<u32>>,
    ) -> BootstrapResult {
        let mut result = BootstrapResult {
            symbols: Vec::new(),
            enrichments: HashMap::new(),
            ref_lines: HashMap::new(),
            name_order: Vec::new(),
        };

        // Build accounted set: definition lines + reference lines from universe
        let mut accounted: HashMap<String, HashSet<u32>> = HashMap::new();
        for defs in by_name.values() {
            for sym in defs {
                accounted
                    .entry(sym.file_path.clone())
                    .or_default()
                    .insert(sym.line + 1);
            }
        }
        for (file, ref_set) in all_ref_lines {
            accounted.entry(file.clone()).or_default().extend(ref_set);
        }

        // Collect unaccounted rg hits: (file, line_1based, matched_text, col)
        let mut unaccounted: Vec<(String, u32, String, u32)> = Vec::new();
        for (file, line_map) in &rg.file_line_texts {
            for (&line, texts) in line_map {
                if accounted.get(file).is_some_and(|s| s.contains(&line)) {
                    continue;
                }
                for (text, col) in texts {
                    unaccounted.push((file.clone(), line, text.clone(), *col));
                }
            }
        }

        if unaccounted.is_empty() {
            return result;
        }

        // Identifier token regex for extracting symbols from matched text
        let Ok(ident_re) = Regex::new(r"[a-zA-Z_]\w*") else {
            return result;
        };

        // Track names we've already bootstrapped to avoid duplicates
        let mut bootstrapped_names: HashSet<String> = HashSet::new();

        // Total distinct symbol count: universe + bootstrapped
        let total_symbols = |result: &BootstrapResult| by_name.len() + result.symbols.len();

        for (file, line_1, matched_text, match_col) in &unaccounted {
            if total_symbols(&result) >= GREP_HOVER_THRESHOLD {
                break;
            }

            // Check if this line is now accounted (by a prior bootstrap round)
            if accounted.get(file).is_some_and(|s| s.contains(line_1)) {
                continue;
            }

            let line_0 = line_1 - 1;
            let path = PathBuf::from(file.as_str());

            // Extract identifier tokens from the matched text
            for m in ident_re.find_iter(matched_text) {
                if total_symbols(&result) >= GREP_HOVER_THRESHOLD {
                    break;
                }

                let token = m.as_str();

                // Skip if already a known symbol name or already bootstrapped
                if by_name.contains_key(token) || bootstrapped_names.contains(token) {
                    continue;
                }

                // Column = match start on line + token offset within match text
                let col = match_col + u32::try_from(m.start()).unwrap_or(0);

                // prepareRename distinguishes symbols from keywords:
                // symbol → range, keyword → null. Cheaper than full enrichment.
                let is_symbol = self.runtime.block_on(async {
                    let Ok((uri_str, client_mutex)) = self.ensure_document_open(&path).await else {
                        return false;
                    };
                    let client = client_mutex.lock().await;
                    if !has_cap(client.capabilities(), "renameProvider") {
                        return true;
                    }
                    let response = client.prepare_rename(&uri_str, line_0, col).await;
                    drop(client);
                    matches!(response, Ok(ref v) if !v.is_null())
                });
                if !is_symbol {
                    continue;
                }

                let (kind, resolved, enrichment) =
                    self.enrich_at_position_infer_kind(&path, line_0, col);

                // resolved_name comes from prepareCallHierarchy (functions)
                // or prepareTypeHierarchy (types). If neither returned a name,
                // this token isn't a structural symbol — skip it.
                let Some(name) = resolved else { continue };
                // If the resolved name doesn't contain the token, this is a
                // keyword (`fn`, `struct`) that resolved to the adjacent symbol
                // — skip and let the real token get enriched. Substring matches
                // (e.g., token `test_glob` resolving to `test_glob_basic`) are
                // legitimate and should proceed.
                if !name.contains(token) {
                    continue;
                }

                // Skip if this resolved name is already known
                if by_name.contains_key(&name) || bootstrapped_names.contains(&name) {
                    // Still update accounted set so remaining hits are explained
                    for (ref_file, ref_lines) in &enrichment.ref_lines {
                        accounted
                            .entry(ref_file.clone())
                            .or_default()
                            .extend(ref_lines);
                        result
                            .ref_lines
                            .entry(ref_file.clone())
                            .or_default()
                            .extend(ref_lines);
                    }
                    accounted.entry(file.clone()).or_default().insert(*line_1);
                    continue;
                }

                // Update accounted set with new reference lines
                for (ref_file, ref_lines) in &enrichment.ref_lines {
                    accounted
                        .entry(ref_file.clone())
                        .or_default()
                        .extend(ref_lines);
                    result
                        .ref_lines
                        .entry(ref_file.clone())
                        .or_default()
                        .extend(ref_lines);
                }

                // Add definition line to accounted
                accounted.entry(file.clone()).or_default().insert(*line_1);

                // Store enrichment
                result
                    .enrichments
                    .insert((file.clone(), line_0), enrichment);

                let sym = SymbolInfo {
                    name: name.clone(),
                    kind,
                    file_path: file.clone(),
                    line: line_0,
                    character: col,
                };

                if !by_name.contains_key(&name) {
                    result.name_order.push(name.clone());
                }
                result.symbols.push(sym);
                bootstrapped_names.insert(name);
            }
        }

        result
    }

    /// Searches workspace roots for pattern matches using the `grep-*` crates
    /// (ripgrep's internals). Walks files in parallel and returns matched
    /// strings and per-file line numbers in a single pass per file.
    ///
    /// # Errors
    ///
    /// Returns an error if the pattern is not a valid regex.
    fn ripgrep_matches(pattern: &str, roots: &[PathBuf]) -> Result<RipgrepMatches> {
        use ignore::WalkState;
        use std::sync::Mutex as StdMutex;

        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(true)
            .build(pattern)
            .map_err(|e| anyhow!("Invalid regex pattern: {e}"))?;

        let collected = Arc::new(StdMutex::new(Vec::<ThreadMatches>::new()));

        for root in roots {
            let walker = WalkBuilder::new(root)
                .git_ignore(true)
                .hidden(false)
                .build_parallel();

            walker.run(|| {
                let matcher = matcher.clone();
                let mut state = CollectOnDrop {
                    local: ThreadMatches::default(),
                    collected: Arc::clone(&collected),
                };

                Box::new(move |entry| {
                    let Ok(entry) = entry else {
                        return WalkState::Continue;
                    };
                    let path = entry.path();
                    if !path.is_file() {
                        return WalkState::Continue;
                    }

                    let path_str = path.to_string_lossy().to_string();
                    let mut sink = MatchSink {
                        matcher: &matcher,
                        path: &path_str,
                        local: &mut state.local,
                    };

                    if let Err(e) = Searcher::new().search_path(&matcher, path, &mut sink) {
                        warn!("grep: skipping {path_str}: {e}");
                    }

                    WalkState::Continue
                })
            });
        }

        let parts = Arc::into_inner(collected)
            .ok_or_else(|| anyhow!("walker threads still hold references"))?
            .into_inner()
            .map_err(|e| anyhow!("lock poisoned: {e}"))?;

        Ok(RipgrepMatches::merge(parts))
    }
}

/// Wrapper that pushes per-thread match data into a shared collector on drop.
/// Each parallel walker thread owns one of these; when `run()` returns and the
/// closures are dropped, each thread's accumulated matches are flushed.
struct CollectOnDrop {
    local: ThreadMatches,
    collected: Arc<std::sync::Mutex<Vec<ThreadMatches>>>,
}

impl Drop for CollectOnDrop {
    fn drop(&mut self) {
        let local = std::mem::take(&mut self.local);
        if local.file_lines.is_empty() && local.matched_set.is_empty() {
            return;
        }
        if let Ok(mut vec) = self.collected.lock() {
            vec.push(local);
        }
    }
}

/// Collects per-file match data for the ripgrep library search.
struct MatchSink<'a> {
    matcher: &'a grep_regex::RegexMatcher,
    path: &'a str,
    local: &'a mut ThreadMatches,
}

impl Sink for MatchSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let Some(line_num) = mat
            .line_number()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n > 0)
        else {
            return Ok(true);
        };

        let line_bytes = mat.bytes();

        // Extract each individual match from the line (--only-matching equivalent)
        let mut at = 0;
        while at < line_bytes.len() {
            let Ok(Some(m)) = self.matcher.find_at(line_bytes, at) else {
                break;
            };
            if m.start() == m.end() {
                // Zero-width match — advance to avoid infinite loop
                at = m.end() + 1;
                continue;
            }
            if let Ok(text) = std::str::from_utf8(&line_bytes[m]) {
                let text = text.to_string();
                let col = u32::try_from(m.start()).unwrap_or(0);
                self.local.matched_set.insert(text.clone());
                self.local
                    .file_line_texts
                    .entry(self.path.to_string())
                    .or_default()
                    .entry(line_num)
                    .or_default()
                    .push((text, col));
            }
            at = m.end();
        }

        self.local
            .file_lines
            .entry(self.path.to_string())
            .or_default()
            .push(line_num);

        Ok(true)
    }
}

impl ToolHandler for LspBridgeHandler {
    fn list_tools(&self) -> Vec<Tool> {
        vec![
            Tool {
                name: "grep".to_string(),
                description: Some(format!("Search for a pattern across the workspace. Queries the full LSP symbol index and ripgrep in parallel. Use `|` for alternation (e.g., `foo|bar`). Returns per-symbol sections with definitions, hover docs, and references (\u{2264}{GREP_HOVER_THRESHOLD} symbols) or name+kind+location (>{GREP_HOVER_THRESHOLD}).")),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern to search for (supports | for alternation)"
                        }
                    },
                    "required": ["pattern"]
                }),
            },
            Tool {
                name: "glob".to_string(),
                description: Some("Browse the workspace. Auto-detects intent: file path → symbol outline, directory path → listing with symbols, glob pattern → matching files with symbols. Always shows outline-level symbols (structs, classes, enums, interfaces, modules, constants).".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "A file path, directory path, or glob pattern (e.g., 'src/', 'src/main.rs', '**/*.rs')"
                        }
                    },
                    "required": ["pattern"]
                }),
            },
            Tool {
                name: "replace".to_string(),
                description: Some("Batch replacement across one or more files.\n\nGLOB (required)\n  File path or glob pattern.\n    src/main.rs          single file\n    src/**/*.rs          all Rust files under src/\n    **/*.md              all markdown files\n\n  Directory paths are not accepted \u{2014} use a glob pattern to match\n  files in a directory (e.g., src/bridge/*.rs).\n\nEDITS (required)\n  Array of {old, new, flags?} replacements applied sequentially.\n\n  old      text to find (literal or regex)\n  new      replacement text ($1, $2, ${name} in regex mode)\n  flags    optional:\n             g  replace all occurrences\n             r  treat old as regex, new supports capture groups\n             i  case insensitive (implies r)\n             m  multiline (implies r)\n             s  dotall (implies r)\n\n  No flags = literal match, first occurrence only (same as Edit).\n\n  Examples:\n    { old: \"OldType\", new: \"NewType\", flags: \"g\" }\n    { old: \"use crate::old\", new: \"use crate::new\" }\n\nLINES (optional)\n  Line ranges to constrain replacements. Space-separated.\n    1-10       lines 1 through 10\n    30         just line 30\n    70-        line 70 through EOF\n\nEXCLUDE (optional)\n  Glob pattern to exclude from matches.\n\nINCLUDE_GITIGNORED (default: false)\n  Include gitignored files in glob expansion.\n\nINCLUDE_HIDDEN (default: false)\n  Include hidden files (dotfiles) in glob expansion.\n\nOUTPUT\n  Per-file replacement count with sample diffs. LSP diagnostics\n  (if any) appear after the summary.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "glob": {
                            "type": "string",
                            "description": "File path or glob pattern"
                        },
                        "edits": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old": { "type": "string", "description": "Text to find" },
                                    "new": { "type": "string", "description": "Replacement text" },
                                    "flags": { "type": "string", "description": "Flags: g (global), r (regex), i, m, s" }
                                },
                                "required": ["old", "new"]
                            },
                            "description": "List of edit operations"
                        },
                        "lines": { "type": "string", "description": "Line ranges (e.g., 1-10 30 70-)" },
                        "exclude": { "type": "string", "description": "Glob pattern to exclude" },
                        "include_gitignored": { "type": "boolean" },
                        "include_hidden": { "type": "boolean" }
                    },
                    "required": ["glob", "edits"]
                }),
            },
        ]
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Replace early dispatch adds necessary branching"
    )]
    fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let start = std::time::Instant::now();
        let file_path = if name == "glob" {
            Self::extract_glob_file_path(arguments.as_ref())
        } else {
            Self::extract_file_path(arguments.as_ref())
        };
        let file = file_path.as_ref().map(|p| p.to_string_lossy().to_string());

        let capture = self.capture_tool_output;
        let params_snapshot = if capture { arguments.clone() } else { None };

        // Broadcast tool call
        self.broadcaster.send(EventKind::ToolCall {
            tool: name.to_string(),
            file,
            params: params_snapshot.clone(),
        });

        // Replace: early dispatch, no LSP readiness wait — handles its own
        // LSP interaction via DiagnosticsServer after the file write.
        if name == "replace" {
            let params = arguments.unwrap_or(serde_json::Value::Null);
            let result = self.runtime.block_on(self.replace.execute(&params, None));

            let (success, output, call_result) = match result {
                Ok(v) => {
                    let text = v.as_str().unwrap_or("").to_string();
                    let output = if capture {
                        if text.is_empty() {
                            None
                        } else {
                            Some(text.clone())
                        }
                    } else {
                        None
                    };
                    (true, output, Ok(CallToolResult::text(text)))
                }
                Err(e) => {
                    let output = if capture { Some(e.to_string()) } else { None };
                    (false, output, Err(e))
                }
            };

            self.broadcaster.send(EventKind::ToolResult {
                tool: name.to_string(),
                success,
                duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                output,
                params: params_snapshot,
            });

            return call_result;
        }

        // Wait for LSP readiness, then check server health.
        // Dead servers are non-fatal — tools degrade gracefully.
        let health = file_path.as_ref().map_or_else(
            || {
                // Symbol-only: wait for all servers
                self.runtime.block_on(self.wait_for_all_servers_ready());
                let touched: Vec<String> = self
                    .runtime
                    .block_on(self.client_manager.active_clients())
                    .keys()
                    .cloned()
                    .collect();
                self.check_server_health(&touched)
            },
            |path| {
                // File-scoped: wait for the specific server
                self.runtime.block_on(self.wait_for_server_ready(path));
                let touched: Vec<String> = self
                    .runtime
                    .block_on(self.language_for_path(path))
                    .into_iter()
                    .collect();
                self.check_server_health(&touched)
            },
        );

        // File-scoped tool with dead server: skip dispatch, return notification
        if !health.dead.is_empty() && file_path.is_some() && name != "glob" {
            let notification = health.notification.unwrap_or_default();
            let output = if capture {
                Some(notification.clone())
            } else {
                None
            };
            self.broadcaster.send(EventKind::ToolResult {
                tool: name.to_string(),
                success: true,
                duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                output,
                params: params_snapshot,
            });
            return Ok(CallToolResult::text(notification));
        }

        // Dispatch tool
        let mut result = match name {
            "grep" => self.handle_grep(arguments),
            "glob" => self.handle_glob(arguments),
            _ => Err(anyhow!("Unknown tool: {name}")),
        };

        // Prepend state-transition notification to the result
        if let Some(note) = health.notification
            && let Ok(ref mut res) = result
        {
            res.content.insert(0, ToolContent::Text { text: note });
        }

        let (success, output) = match &result {
            Ok(res) => {
                let output = if capture {
                    let text: String = res
                        .content
                        .iter()
                        .map(|c| {
                            let ToolContent::Text { text } = c;
                            text.as_str()
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if text.is_empty() { None } else { Some(text) }
                } else {
                    None
                };
                (res.is_error.is_none(), output)
            }
            Err(e) => (false, if capture { Some(e.to_string()) } else { None }),
        };

        self.broadcaster.send(EventKind::ToolResult {
            tool: name.to_string(),
            success,
            duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
            output,
            params: params_snapshot,
        });

        result
    }
}

// ─── Search enrichment types and formatting ─────────────────────────────

/// Enrichment data collected for a single workspace symbol.
#[derive(Default)]
struct SymbolEnrichment {
    /// Hover content (signature + docs).
    hover: Option<String>,
    /// Reference line numbers per file (for deduplication).
    ref_lines: HashMap<String, HashSet<u32>>,
    /// Incoming calls: `(caller_name, file_path, line_1based)`.
    incoming_calls: Vec<(String, String, u32)>,
    /// Implementation locations: `(file_path, line_1based)`.
    implementations: Vec<(String, u32)>,
    /// Subtypes: `(type_name, file_path, line_1based)`.
    subtypes: Vec<(String, String, u32)>,
}

/// Symbols and enrichment discovered by rg-bootstrapped hover.
struct BootstrapResult {
    /// Newly-discovered `SymbolInfo` values.
    symbols: Vec<SymbolInfo>,
    /// Enrichment keyed by `(file_path, line_0based)`.
    enrichments: HashMap<(String, u32), SymbolEnrichment>,
    /// Reference lines discovered during bootstrap, keyed by file path.
    ref_lines: HashMap<String, HashSet<u32>>,
    /// Names of bootstrapped symbols in discovery order.
    name_order: Vec<String>,
}

/// Result of a ripgrep `--only-matching` search.
#[derive(Default)]
struct RipgrepMatches {
    /// Unique matched strings (for LSP queries).
    matched_strings: Vec<String>,
    /// Per-file line numbers (for heatmap tier).
    file_lines: BTreeMap<String, Vec<u32>>,
    /// Per-file, per-line matched texts with column offsets
    /// `(matched_text, column_byte_offset)` for routing unclaimed lines
    /// to headings and for rg-bootstrapped hover positions.
    file_line_texts: HashMap<String, HashMap<u32, Vec<(String, u32)>>>,
}

impl RipgrepMatches {
    /// Merges per-thread match accumulators into a single result.
    fn merge(parts: Vec<ThreadMatches>) -> Self {
        let mut file_lines: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        let mut matched_set: HashSet<String> = HashSet::new();
        let mut file_line_texts: HashMap<String, HashMap<u32, Vec<(String, u32)>>> = HashMap::new();

        for part in parts {
            for (file, lines) in part.file_lines {
                file_lines.entry(file).or_default().extend(lines);
            }
            matched_set.extend(part.matched_set);
            for (file, line_map) in part.file_line_texts {
                let entry = file_line_texts.entry(file).or_default();
                for (line, texts) in line_map {
                    entry.entry(line).or_default().extend(texts);
                }
            }
        }

        Self {
            matched_strings: matched_set.into_iter().collect(),
            file_lines,
            file_line_texts,
        }
    }
}

/// Per-thread match accumulator used during parallel file walking.
#[derive(Default)]
struct ThreadMatches {
    /// Per-file line numbers.
    file_lines: BTreeMap<String, Vec<u32>>,
    /// Unique matched strings (`HashSet` for efficient per-thread dedup).
    matched_set: HashSet<String>,
    /// Per-file, per-line matched texts with column offsets.
    file_line_texts: HashMap<String, HashMap<u32, Vec<(String, u32)>>>,
}

/// Returns `true` if a capability key is present and non-null.
fn has_cap(caps: &Value, key: &str) -> bool {
    caps.get(key).is_some_and(|v| !v.is_null())
}

/// Extracts plain text from a `Value`-based LSP hover response.
fn extract_hover_text_from_value(hover: &Value) -> Option<String> {
    let contents = hover.get("contents")?;

    // String: plain MarkedString
    if let Some(s) = contents.as_str() {
        let s = s.trim();
        return if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        };
    }

    // Object with "value" field: MarkupContent or LanguageString MarkedString
    if let Some(value) = contents.get("value").and_then(Value::as_str) {
        let text = value.trim();
        return if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        };
    }

    // Array of MarkedString
    if let Some(arr) = contents.as_array() {
        let texts: Vec<String> = arr
            .iter()
            .filter_map(|item| {
                item.as_str().map_or_else(
                    || {
                        item.get("value")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    },
                    |s| Some(s.to_string()),
                )
            })
            .collect();
        return if texts.is_empty() {
            None
        } else {
            Some(texts.join("\n"))
        };
    }

    None
}

/// Extracts the file path from a hierarchy item's `uri` field.
fn value_file_path(item: &Value) -> String {
    item.get("uri")
        .and_then(Value::as_str)
        .and_then(symbols::uri_to_path)
        .unwrap_or_default()
}

/// Extracts `range.start.line` from a hierarchy item `Value`.
fn value_start_line(item: &Value) -> u32 {
    item.get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0)
}

/// Extracts incoming calls from a `callHierarchy/incomingCalls` response
/// into the enrichment's `incoming_calls` list.
fn extract_incoming_calls(response: &Value, enrichment: &mut SymbolEnrichment) {
    if let Some(calls) = response.as_array() {
        for call in calls {
            if let Some(from) = call.get("from") {
                enrichment.incoming_calls.push((
                    from.get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    value_file_path(from),
                    value_start_line(from) + 1,
                ));
            }
        }
    }
}

/// Extracts subtypes from a `typeHierarchy/subtypes` response
/// into the enrichment's `subtypes` list.
fn extract_subtypes(response: &Value, enrichment: &mut SymbolEnrichment) {
    if let Some(subs) = response.as_array() {
        for sub in subs {
            enrichment.subtypes.push((
                sub.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                value_file_path(sub),
                value_start_line(sub) + 1,
            ));
        }
    }
}

/// Makes a file path relative to the nearest root, for display.
fn display_path(file: &str, roots: &[PathBuf]) -> String {
    roots
        .iter()
        .find_map(|root| {
            let root_str = root.to_string_lossy();
            file.strip_prefix(root_str.as_ref())
                .map(|rest| rest.strip_prefix('/').unwrap_or(rest).to_string())
        })
        .unwrap_or_else(|| file.to_string())
}

/// Formats sorted line numbers as compact ranges: `L45`, `L15-L20`.
/// Nearby lines are clustered using sqrt-based merge distance (DBSCAN-style):
/// `merge_distance = ceil(sqrt(max_line))`. This scales with file size —
/// small files cluster tightly, large files tolerate wider gaps.
fn format_line_ranges(lines: &[u32]) -> String {
    if lines.is_empty() {
        return String::new();
    }

    let mut sorted = lines.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let max_line = sorted[sorted.len() - 1];
    // Integer ceiling of sqrt: isqrt rounds down, so add 1 unless it's a perfect square.
    let isqrt = u32::isqrt(max_line);
    let merge_distance = if isqrt * isqrt == max_line {
        isqrt
    } else {
        isqrt + 1
    }
    .max(1);

    let mut ranges: Vec<String> = Vec::new();
    let mut start = sorted[0];
    let mut end = sorted[0];

    for &line in &sorted[1..] {
        if line - end > merge_distance {
            ranges.push(format_single_range(start, end));
            start = line;
        }
        end = line;
    }
    ranges.push(format_single_range(start, end));

    ranges.join(" ")
}

/// Formats a single line or range.
fn format_single_range(start: u32, end: u32) -> String {
    if start == end {
        format!("L{start}")
    } else {
        format!("L{start}-L{end}")
    }
}

/// Formats references from enrichment, excluding the definition line
/// and any lines already shown in labeled sections.
fn format_symbol_references(
    enrichment: &SymbolEnrichment,
    def_file: &str,
    def_line_0: u32,
    roots: &[PathBuf],
    labeled_lines: &HashSet<(String, u32)>,
) -> String {
    use std::fmt::Write;

    let def_line_1 = def_line_0 + 1;
    let mut output = String::new();

    // Sort files for stable output
    let mut files: Vec<&String> = enrichment.ref_lines.keys().collect();
    files.sort();

    for file in files {
        let lines = &enrichment.ref_lines[file];
        // Filter out the definition line and labeled lines
        let is_def_file = file.as_str() == def_file;
        let mut filtered: Vec<u32> = lines
            .iter()
            .copied()
            .filter(|&l| {
                if is_def_file && l == def_line_1 {
                    return false;
                }
                !labeled_lines.contains(&(file.clone(), l))
            })
            .collect();
        if filtered.is_empty() {
            continue;
        }
        filtered.sort_unstable();
        let path = display_path(file, roots);
        let _ = writeln!(output, "{path} {}", format_line_ranges(&filtered));
    }

    let trimmed_len = output.trim_end().len();
    output.truncate(trimmed_len);
    output
}

/// Assigns rg file/line hits to symbol names based on LSP reference data
/// and matched text routing.
///
/// Returns a map from heading name to file hits. Each heading name is either
/// an LSP symbol name or an rg-only matched string.
fn assign_rg_lines_to_symbols(
    by_name: &BTreeMap<String, Vec<&SymbolInfo>>,
    all_ref_lines: &HashMap<String, HashSet<u32>>,
    rg: &RipgrepMatches,
) -> BTreeMap<String, Vec<(String, Vec<u32>)>> {
    let mut result: BTreeMap<String, Vec<(String, Vec<u32>)>> = BTreeMap::new();

    // Track which rg lines are claimed by any symbol's references
    let mut claimed: HashMap<String, HashSet<u32>> = HashMap::new();
    for (file, ref_set) in all_ref_lines {
        claimed.entry(file.clone()).or_default().extend(ref_set);
    }

    // Also claim definition lines
    for defs in by_name.values() {
        for sym in defs {
            claimed
                .entry(sym.file_path.clone())
                .or_default()
                .insert(sym.line + 1);
        }
    }

    // Build a lowercase lookup for symbol names
    let name_lower: Vec<(String, String)> = by_name
        .keys()
        .map(|n: &String| (n.clone(), n.to_lowercase()))
        .collect();

    // For each rg file, find unclaimed lines and route by matched text
    for (file, lines) in &rg.file_lines {
        let unclaimed: Vec<u32> = lines
            .iter()
            .copied()
            .filter(|l| !claimed.get(file.as_str()).is_some_and(|s| s.contains(l)))
            .collect();
        if unclaimed.is_empty() {
            continue;
        }

        // Group unclaimed lines by their matched text, then route to headings
        let file_texts = rg.file_line_texts.get(file.as_str());
        // Collect (heading, lines) per heading for this file
        let mut heading_lines: BTreeMap<String, Vec<u32>> = BTreeMap::new();

        for &line_num in &unclaimed {
            let texts = file_texts.and_then(|ft| ft.get(&line_num));
            let mut routed = false;

            if let Some(texts) = texts {
                for (matched_text, _col) in texts {
                    let mt_lower = matched_text.to_lowercase();
                    // Try to match to a known symbol name
                    for (name, nl) in &name_lower {
                        if mt_lower == *nl || mt_lower.contains(nl.as_str()) {
                            heading_lines
                                .entry(name.clone())
                                .or_default()
                                .push(line_num);
                            routed = true;
                            break;
                        }
                    }
                    if !routed {
                        // No symbol match — use the matched text itself as heading
                        heading_lines
                            .entry(matched_text.clone())
                            .or_default()
                            .push(line_num);
                        routed = true;
                    }
                    if routed {
                        break;
                    }
                }
            }

            if !routed {
                // No matched text info — fallback to empty key
                heading_lines
                    .entry(String::new())
                    .or_default()
                    .push(line_num);
            }
        }

        for (heading, lines) in heading_lines {
            result
                .entry(heading)
                .or_default()
                .push((file.clone(), lines));
        }
    }

    result
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    // ─── display_path tests ──────────────────────────────────────────────

    #[test]
    fn test_display_path_strips_root() {
        let roots = vec![PathBuf::from("/home/user/project")];
        assert_eq!(
            display_path("/home/user/project/src/main.rs", &roots),
            "src/main.rs"
        );
    }

    #[test]
    fn test_display_path_no_matching_root() {
        let roots = vec![PathBuf::from("/home/user/project")];
        assert_eq!(
            display_path("/other/path/file.rs", &roots),
            "/other/path/file.rs"
        );
    }

    // ─── format_line_ranges tests ────────────────────────────────────────

    #[test]
    fn test_format_line_ranges_empty() {
        assert_eq!(format_line_ranges(&[]), "");
    }

    #[test]
    fn test_format_line_ranges_single() {
        assert_eq!(format_line_ranges(&[42]), "L42");
    }

    #[test]
    fn test_format_line_ranges_consecutive() {
        assert_eq!(format_line_ranges(&[10, 11, 12]), "L10-L12");
    }

    #[test]
    fn test_format_line_ranges_disjoint() {
        // max=20, merge_distance=ceil(sqrt(20))=5
        // gap 5→10 = 5 ≤ 5 → merge; gap 10→20 = 10 > 5 → split
        assert_eq!(format_line_ranges(&[5, 10, 20]), "L5-L10 L20");
    }

    #[test]
    fn test_format_line_ranges_mixed() {
        // max=50, merge_distance=ceil(sqrt(50))=8
        // gaps: 3→10=7 ≤ 8, 11→50=39 > 8
        assert_eq!(format_line_ranges(&[1, 2, 3, 10, 11, 50]), "L1-L11 L50");
    }

    #[test]
    fn test_format_line_ranges_unsorted() {
        // max=20, merge_distance=5; sorted [10,11,20]; gap 11→20=9 > 5
        assert_eq!(format_line_ranges(&[20, 10, 11]), "L10-L11 L20");
    }

    #[test]
    fn test_format_line_ranges_dbscan_nearby() {
        // max=30, merge_distance=ceil(sqrt(30))=6; gap 25→30=5 ≤ 6
        assert_eq!(format_line_ranges(&[25, 30]), "L25-L30");
    }

    #[test]
    fn test_format_line_ranges_dbscan_far_apart() {
        // max=1000, merge_distance=ceil(sqrt(1000))=32; gap=999 > 32
        assert_eq!(format_line_ranges(&[1, 1000]), "L1 L1000");
    }

    #[test]
    fn test_format_line_ranges_dbscan_mixed() {
        // max=14, merge_distance=ceil(sqrt(14))=4
        // gap 5→10=5 > 4 → split; gap 10→14=4 ≤ 4 → merge
        assert_eq!(format_line_ranges(&[5, 10, 14]), "L5 L10-L14");
    }

    // ─── format_symbol_references tests ──────────────────────────────────

    #[test]
    fn test_format_symbol_references_excludes_def() {
        let mut ref_lines = HashMap::new();
        ref_lines.insert("/src/lib.rs".to_string(), HashSet::from([1, 10, 20]));

        let enrichment = SymbolEnrichment {
            hover: None,
            ref_lines,
            ..SymbolEnrichment::default()
        };
        let roots = vec![PathBuf::from("/")];
        let labeled = HashSet::new();

        // Definition is at line 0 (0-indexed) = line 1 (1-indexed)
        let result = format_symbol_references(&enrichment, "/src/lib.rs", 0, &roots, &labeled);
        assert!(result.contains("L10"));
        assert!(result.contains("L20"));
        assert!(
            !result.contains("L1 "),
            "Definition line should be excluded"
        );
    }
}

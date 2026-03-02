// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Bridge handler that maps MCP tool calls to LSP requests.

use anyhow::{Result, anyhow};
use lsp_types::{
    CallHierarchyIncomingCallsParams, CallHierarchyPrepareParams, GotoDefinitionParams,
    GotoDefinitionResponse, HoverParams, ReferenceContext, ReferenceParams, SymbolInformation,
    SymbolKind, TextDocumentIdentifier, TextDocumentPositionParams, TypeHierarchyPrepareParams,
    TypeHierarchySubtypesParams, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use regex::Regex;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::lsp::{ClientManager, LspClient};
use crate::mcp::{CallToolResult, Tool, ToolContent, ToolHandler};
use crate::session::{EventBroadcaster, EventKind};

/// Maximum unique LSP symbols for full enrichment (hover + references +
/// labeled incoming calls / implementations / subtypes).
/// Above this threshold, symbols are rendered with name + kind + location only.
const GREP_ENRICHMENT_THRESHOLD: usize = 10;

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
}

impl LspBridgeHandler {
    /// Creates a new `LspBridgeHandler`.
    pub fn new(
        client_manager: Arc<ClientManager>,
        doc_manager: Arc<Mutex<DocumentManager>>,
        runtime: Handle,
        broadcaster: EventBroadcaster,
    ) -> Self {
        Self {
            client_manager,
            doc_manager,
            runtime,
            broadcaster,
            notified_offline: std::sync::Mutex::new(HashSet::new()),
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
    ) -> Result<(lsp_types::Uri, Arc<Mutex<LspClient>>)> {
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
                DocumentNotification::Open(params) => {
                    client.did_open(params).await?;
                }
                DocumentNotification::Change(params) => {
                    client.did_change(params).await?;
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
        let rg = Self::ripgrep_matches(&input.pattern, &roots);

        // 2. Symbol universe: workspace/symbol("") + regex filter, with rg fallback
        let symbols = self.runtime.block_on(async {
            let clients = self.client_manager.active_clients().await;

            // Try workspace/symbol("") first — returns the full symbol index
            let mut all_symbols: Vec<SymbolInformation> =
                self.fetch_symbol_universe(&clients).await;

            // Fallback: if symbol("") returned nothing, use rg matched strings
            if all_symbols.is_empty() && !rg.matched_strings.is_empty() {
                all_symbols = self
                    .fetch_symbols_by_queries(&rg.matched_strings, &clients)
                    .await;
            }

            // Regex filter against the user's pattern
            all_symbols.retain(|s| re.is_match(&s.name));

            // Dedupe by (name, uri, line)
            let mut seen: HashSet<(String, String, u32)> = HashSet::new();
            all_symbols.retain(|s| {
                seen.insert((
                    s.name.clone(),
                    s.location.uri.to_string(),
                    s.location.range.start.line,
                ))
            });

            all_symbols
        });

        let enrich = symbols.len() <= GREP_ENRICHMENT_THRESHOLD;

        // 3. Group symbols by name (preserving order of first occurrence)
        let mut name_order: Vec<String> = Vec::new();
        let mut by_name: BTreeMap<String, Vec<&SymbolInformation>> = BTreeMap::new();
        for sym in &symbols {
            if !by_name.contains_key(&sym.name) {
                name_order.push(sym.name.clone());
            }
            by_name.entry(sym.name.clone()).or_default().push(sym);
        }

        // 4. Collect enrichment data and build per-symbol ref lines for rg dedup
        let mut enrichments: HashMap<(String, u32), SymbolEnrichment> = HashMap::new();
        let mut all_ref_lines: HashMap<String, HashSet<u32>> = HashMap::new();

        if enrich {
            for sym in &symbols {
                let enrichment = self.enrich_symbol(sym);
                for (file, lines) in &enrichment.ref_lines {
                    all_ref_lines.entry(file.clone()).or_default().extend(lines);
                }
                let key = (
                    sym.location.uri.path().to_string(),
                    sym.location.range.start.line,
                );
                enrichments.insert(key, enrichment);
            }
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

            // Non-code rg hits for this heading
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
                    let path = display_path(sym.location.uri.path().as_str(), &roots);
                    let line = sym.location.range.start.line + 1;
                    let _ = writeln!(output, "\n## [{kind}] {path}:{line}");

                    if enrich {
                        let key = (
                            sym.location.uri.path().to_string(),
                            sym.location.range.start.line,
                        );
                        if let Some(enrichment) = enrichments.get(&key) {
                            // Hover
                            if let Some(hover) = &enrichment.hover {
                                let _ = writeln!(output, "\n{hover}");
                            }

                            // Labeled sections + collect labeled lines for dedup
                            let mut labeled_lines: HashSet<(String, u32)> = HashSet::new();

                            if !enrichment.incoming_calls.is_empty() {
                                let _ = writeln!(output, "\nCalled by:");
                                for (name, file, line) in &enrichment.incoming_calls {
                                    let path = display_path(file, &roots);
                                    let _ = writeln!(output, "  {name}  {path}:{line}");
                                    labeled_lines.insert((file.clone(), *line));
                                }
                            }

                            if !enrichment.implementations.is_empty() {
                                let _ = writeln!(output, "\nImplementations:");
                                for (file, line) in &enrichment.implementations {
                                    let path = display_path(file, &roots);
                                    let _ = writeln!(output, "  {path}:{line}");
                                    labeled_lines.insert((file.clone(), *line));
                                }
                            }

                            if !enrichment.subtypes.is_empty() {
                                let _ = writeln!(output, "\nSubtypes:");
                                for (name, file, line) in &enrichment.subtypes {
                                    let path = display_path(file, &roots);
                                    let _ = writeln!(output, "  {name}  {path}:{line}");
                                    labeled_lines.insert((file.clone(), *line));
                                }
                            }

                            // References (excluding definition line and labeled lines)
                            let ref_output = format_symbol_references(
                                enrichment,
                                sym.location.uri.path().as_str(),
                                sym.location.range.start.line,
                                &roots,
                                &labeled_lines,
                            );
                            if !ref_output.is_empty() {
                                let _ = writeln!(output, "\n{ref_output}");
                            }
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
    ) -> Vec<SymbolInformation> {
        let params = WorkspaceSymbolParams {
            query: String::new(),
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        };

        let mut all_symbols: Vec<SymbolInformation> = Vec::new();

        for client_mutex in clients.values() {
            let client = client_mutex.lock().await;
            let supports_resolve = client.supports_workspace_symbol_resolve();

            let Ok(Some(response)) = client.workspace_symbols(params.clone()).await else {
                continue;
            };

            match response {
                WorkspaceSymbolResponse::Flat(flat) => all_symbols.extend(flat),
                WorkspaceSymbolResponse::Nested(nested) => {
                    for ws in nested {
                        match ws.location {
                            lsp_types::OneOf::Left(ref location) => {
                                #[allow(deprecated, reason = "LSP spec uses deprecated fields")]
                                all_symbols.push(SymbolInformation {
                                    name: ws.name.clone(),
                                    kind: ws.kind,
                                    tags: ws.tags.clone(),
                                    deprecated: None,
                                    location: location.clone(),
                                    container_name: ws.container_name.clone(),
                                });
                            }
                            lsp_types::OneOf::Right(_) if supports_resolve => {
                                // URI-only: resolve to get full location
                                if let Ok(Some(resolved)) =
                                    client.workspace_symbol_resolve(ws.clone()).await
                                    && let lsp_types::OneOf::Left(location) = resolved.location
                                {
                                    #[allow(
                                        deprecated,
                                        reason = "LSP spec uses deprecated fields"
                                    )]
                                    all_symbols.push(SymbolInformation {
                                        name: resolved.name,
                                        kind: resolved.kind,
                                        tags: resolved.tags,
                                        deprecated: None,
                                        location,
                                        container_name: resolved.container_name,
                                    });
                                }
                            }
                            lsp_types::OneOf::Right(_) => {
                                // URI-only but no resolve support — skip
                            }
                        }
                    }
                }
            }
        }

        all_symbols
    }

    /// Fallback: queries workspace/symbol with each matched string (ticket 08 behavior).
    /// Handles `OneOf::Right` (URI-only) symbols via resolve when supported.
    async fn fetch_symbols_by_queries(
        &self,
        queries: &[String],
        clients: &HashMap<String, Arc<Mutex<LspClient>>>,
    ) -> Vec<SymbolInformation> {
        let mut all_symbols: Vec<SymbolInformation> = Vec::new();

        for query in queries {
            let params = WorkspaceSymbolParams {
                query: query.clone(),
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            };

            for client_mutex in clients.values() {
                let client = client_mutex.lock().await;
                let supports_resolve = client.supports_workspace_symbol_resolve();

                let Ok(Some(response)) = client.workspace_symbols(params.clone()).await else {
                    continue;
                };

                match response {
                    WorkspaceSymbolResponse::Flat(flat) => all_symbols.extend(flat),
                    WorkspaceSymbolResponse::Nested(nested) => {
                        for ws in nested {
                            match ws.location {
                                lsp_types::OneOf::Left(ref location) => {
                                    #[allow(
                                        deprecated,
                                        reason = "LSP spec uses deprecated fields"
                                    )]
                                    all_symbols.push(SymbolInformation {
                                        name: ws.name.clone(),
                                        kind: ws.kind,
                                        tags: ws.tags.clone(),
                                        deprecated: None,
                                        location: location.clone(),
                                        container_name: ws.container_name.clone(),
                                    });
                                }
                                lsp_types::OneOf::Right(_) if supports_resolve => {
                                    if let Ok(Some(resolved)) =
                                        client.workspace_symbol_resolve(ws.clone()).await
                                        && let lsp_types::OneOf::Left(location) = resolved.location
                                    {
                                        #[allow(
                                            deprecated,
                                            reason = "LSP spec uses deprecated fields"
                                        )]
                                        all_symbols.push(SymbolInformation {
                                            name: resolved.name,
                                            kind: resolved.kind,
                                            tags: resolved.tags,
                                            deprecated: None,
                                            location,
                                            container_name: resolved.container_name,
                                        });
                                    }
                                }
                                lsp_types::OneOf::Right(_) => {
                                    // URI-only but no resolve support — skip
                                }
                            }
                        }
                    }
                }
            }
        }

        all_symbols
    }

    /// Enriches a symbol with hover, references, and kind-specific labels.
    #[allow(clippy::too_many_lines, reason = "Sequential LSP calls by kind")]
    fn enrich_symbol(&self, sym: &SymbolInformation) -> SymbolEnrichment {
        let path = PathBuf::from(sym.location.uri.path().as_str());
        let position = sym.location.range.start;
        let kind = sym.kind;

        self.runtime.block_on(async {
            let mut enrichment = SymbolEnrichment::default();

            let Ok((uri, client_mutex)) = self.ensure_document_open(&path).await else {
                return enrichment;
            };

            let client = client_mutex.lock().await;
            let caps = client.capabilities();

            // Hover — signature + docs
            if caps.hover_provider.is_some() {
                let params = HoverParams {
                    text_document_position_params: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier { uri: uri.clone() },
                        position,
                    },
                    work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                };
                if let Ok(Some(hover)) = client.hover(params).await {
                    enrichment.hover = extract_hover_text(&hover);
                }
            }

            // References — collect line numbers for dedup
            if caps.references_provider.is_some() {
                let params = ReferenceParams {
                    text_document_position: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier { uri: uri.clone() },
                        position,
                    },
                    work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                    partial_result_params: lsp_types::PartialResultParams::default(),
                    context: ReferenceContext {
                        include_declaration: true,
                    },
                };
                if let Ok(Some(refs)) = client.references(params).await {
                    for loc in &refs {
                        enrichment
                            .ref_lines
                            .entry(loc.uri.path().to_string())
                            .or_default()
                            .insert(loc.range.start.line + 1);
                    }
                }
            }

            // Kind-specific enrichment
            match kind {
                // Functions/methods/constructors → incoming calls
                SymbolKind::FUNCTION | SymbolKind::METHOD | SymbolKind::CONSTRUCTOR => {
                    if caps.call_hierarchy_provider.is_some() {
                        let prepare_params = CallHierarchyPrepareParams {
                            text_document_position_params: TextDocumentPositionParams {
                                text_document: TextDocumentIdentifier { uri: uri.clone() },
                                position,
                            },
                            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                        };
                        if let Ok(Some(items)) = client.prepare_call_hierarchy(prepare_params).await
                        {
                            for item in items {
                                let params = CallHierarchyIncomingCallsParams {
                                    item,
                                    work_done_progress_params:
                                        lsp_types::WorkDoneProgressParams::default(),
                                    partial_result_params: lsp_types::PartialResultParams::default(
                                    ),
                                };
                                if let Ok(Some(calls)) = client.incoming_calls(params).await {
                                    for call in calls {
                                        enrichment.incoming_calls.push((
                                            call.from.name,
                                            call.from.uri.path().to_string(),
                                            call.from.range.start.line + 1,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }

                // Structs/classes/enums → implementations
                SymbolKind::STRUCT | SymbolKind::CLASS | SymbolKind::ENUM => {
                    if caps.implementation_provider.is_some() {
                        let params = GotoDefinitionParams {
                            text_document_position_params: TextDocumentPositionParams {
                                text_document: TextDocumentIdentifier { uri: uri.clone() },
                                position,
                            },
                            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                            partial_result_params: lsp_types::PartialResultParams::default(),
                        };
                        if let Ok(Some(response)) = client.implementation(params).await {
                            for loc in goto_definition_locations(&response) {
                                enrichment
                                    .implementations
                                    .push((loc.uri.path().to_string(), loc.range.start.line + 1));
                            }
                        }
                    }
                }

                // Interfaces/traits → subtypes
                SymbolKind::INTERFACE => {
                    if client.supports_type_hierarchy() {
                        let prepare_params = TypeHierarchyPrepareParams {
                            text_document_position_params: TextDocumentPositionParams {
                                text_document: TextDocumentIdentifier { uri: uri.clone() },
                                position,
                            },
                            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                        };
                        if let Ok(Some(items)) = client.prepare_type_hierarchy(prepare_params).await
                        {
                            for item in items {
                                let params = TypeHierarchySubtypesParams {
                                    item,
                                    work_done_progress_params:
                                        lsp_types::WorkDoneProgressParams::default(),
                                    partial_result_params: lsp_types::PartialResultParams::default(
                                    ),
                                };
                                if let Ok(Some(sub_items)) = client.subtypes(params).await {
                                    for sub in sub_items {
                                        enrichment.subtypes.push((
                                            sub.name,
                                            sub.uri.path().to_string(),
                                            sub.range.start.line + 1,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }

                _ => {}
            }

            enrichment
        })
    }

    /// Runs ripgrep with `--only-matching` and returns both matched strings
    /// and per-file line numbers.
    fn ripgrep_matches(pattern: &str, roots: &[PathBuf]) -> RipgrepMatches {
        use std::process::Command;

        let mut cmd = Command::new("rg");
        cmd.args([
            "--line-number",
            "--no-heading",
            "--ignore-case",
            "--only-matching",
            pattern,
        ]);

        for root in roots {
            cmd.arg(root);
        }

        let Ok(rg_output) = cmd.output() else {
            return RipgrepMatches::default();
        };

        if !rg_output.status.success() && rg_output.stdout.is_empty() {
            return RipgrepMatches::default();
        }

        let mut file_lines: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        let mut matched_set: HashSet<String> = HashSet::new();
        let mut file_line_texts: HashMap<String, HashMap<u32, Vec<String>>> = HashMap::new();
        let stdout = String::from_utf8_lossy(&rg_output.stdout);

        // --only-matching output format: file:line:matched_text
        for line in stdout.lines() {
            let Some((file, rest)) = line.split_once(':') else {
                continue;
            };
            let Some((line_str, matched_text)) = rest.split_once(':') else {
                continue;
            };
            let Ok(line_num) = line_str.parse::<u32>() else {
                continue;
            };

            file_lines
                .entry(file.to_string())
                .or_default()
                .push(line_num);
            matched_set.insert(matched_text.to_string());
            file_line_texts
                .entry(file.to_string())
                .or_default()
                .entry(line_num)
                .or_default()
                .push(matched_text.to_string());
        }

        RipgrepMatches {
            matched_strings: matched_set.into_iter().collect(),
            file_lines,
            file_line_texts,
        }
    }
}

impl ToolHandler for LspBridgeHandler {
    fn list_tools(&self) -> Vec<Tool> {
        vec![
            Tool {
                name: "grep".to_string(),
                description: Some("Search for a pattern across the workspace. Queries the full LSP symbol index and ripgrep in parallel. Use `|` for alternation (e.g., `foo|bar`). Returns per-symbol sections with definitions, hover docs, and references (≤10 symbols) or name+kind+location (>10).".to_string()),
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
        ]
    }

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

        // Broadcast tool call
        self.broadcaster.send(EventKind::ToolCall {
            tool: name.to_string(),
            file,
        });

        // Helper to broadcast result
        let broadcast_result = |success: bool| {
            self.broadcaster.send(EventKind::ToolResult {
                tool: name.to_string(),
                success,
                duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
            });
        };

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
            broadcast_result(true);
            return Ok(CallToolResult::text(
                health.notification.unwrap_or_default(),
            ));
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

        match &result {
            Ok(res) => broadcast_result(res.is_error.is_none()),
            Err(_) => broadcast_result(false),
        }

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

/// Result of a ripgrep `--only-matching` search.
#[derive(Default)]
struct RipgrepMatches {
    /// Unique matched strings (for LSP queries).
    matched_strings: Vec<String>,
    /// Per-file line numbers (for heatmap tier).
    file_lines: BTreeMap<String, Vec<u32>>,
    /// Per-file, per-line matched texts (for routing unclaimed lines to headings).
    file_line_texts: HashMap<String, HashMap<u32, Vec<String>>>,
}

/// Extracts plain text from an LSP `Hover` response.
fn extract_hover_text(hover: &lsp_types::Hover) -> Option<String> {
    match &hover.contents {
        lsp_types::HoverContents::Scalar(content) => Some(markup_content_to_string(content)),
        lsp_types::HoverContents::Array(contents) => {
            let texts: Vec<String> = contents.iter().map(markup_content_to_string).collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n"))
            }
        }
        lsp_types::HoverContents::Markup(markup) => {
            let text = markup.value.trim().to_string();
            if text.is_empty() { None } else { Some(text) }
        }
    }
}

/// Converts a `MarkedString` to plain text.
fn markup_content_to_string(content: &lsp_types::MarkedString) -> String {
    match content {
        lsp_types::MarkedString::String(s) => s.clone(),
        lsp_types::MarkedString::LanguageString(ls) => ls.value.clone(),
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

/// Formats a `SymbolKind` as a human-readable string.
fn format_symbol_kind(kind: SymbolKind) -> String {
    format!("{kind:?}")
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

/// Extracts locations from a `GotoDefinitionResponse`.
fn goto_definition_locations(response: &GotoDefinitionResponse) -> Vec<lsp_types::Location> {
    match response {
        GotoDefinitionResponse::Scalar(loc) => vec![loc.clone()],
        GotoDefinitionResponse::Array(locs) => locs.clone(),
        GotoDefinitionResponse::Link(links) => links
            .iter()
            .map(|link| lsp_types::Location {
                uri: link.target_uri.clone(),
                range: link.target_range,
            })
            .collect(),
    }
}

/// Assigns rg file/line hits to symbol names based on LSP reference data
/// and matched text routing.
///
/// Returns a map from heading name to file hits. Each heading name is either
/// an LSP symbol name or an rg-only matched string.
fn assign_rg_lines_to_symbols(
    by_name: &BTreeMap<String, Vec<&SymbolInformation>>,
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
            let file = sym.location.uri.path().to_string();
            let line = sym.location.range.start.line + 1;
            claimed.entry(file).or_default().insert(line);
        }
    }

    // Build a lowercase lookup for symbol names
    let name_lower: Vec<(String, String)> = by_name
        .keys()
        .map(|n| (n.clone(), n.to_lowercase()))
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
                for matched_text in texts {
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

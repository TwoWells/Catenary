// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Bridge handler that maps MCP tool calls to LSP requests.

use anyhow::{Result, anyhow};
use ignore::WalkBuilder;
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyOutgoingCall,
    CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams, Diagnostic, DiagnosticSeverity,
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams,
    GotoDefinitionResponse, HoverParams, Location, Position, ReferenceContext, ReferenceParams,
    SymbolInformation, SymbolKind, TextDocumentIdentifier, TextDocumentPositionParams,
    TypeHierarchyItem, TypeHierarchyPrepareParams, TypeHierarchySubtypesParams,
    TypeHierarchySupertypesParams, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::lsp::{ClientManager, DiagnosticsWaitResult, LspClient};
use crate::mcp::{CallToolResult, Tool, ToolContent, ToolHandler};
use crate::session::{EventBroadcaster, EventKind};

/// Tools that do not require LSP server readiness.
/// Everything else waits by default — new tools are safe automatically.
const METHODS_SKIP_WAIT: &[&str] = &["list_directory"];

/// Maximum total references before disambiguation falls back to flat rendering.
const DISAMBIGUATION_REF_LIMIT: usize = 200;

use super::{DocumentManager, DocumentNotification};

/// Result of a server health check against touched language servers.
struct ServerHealth {
    /// Languages with dead servers.
    dead: Vec<String>,
    /// One-time batched notification for state transitions (offline/recovery).
    notification: Option<String>,
}

/// Controls how much symbol detail to include in output.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DetailLevel {
    /// Only structural symbols: modules, classes, structs, interfaces, enums.
    #[default]
    Outline,
    /// Outline + functions, methods, constructors.
    Signatures,
    /// Everything including variables, constants, fields.
    Full,
}

const fn default_detail_level() -> DetailLevel {
    DetailLevel::Outline
}

/// Input for tools that need only a file path.
#[derive(Debug, Deserialize)]
pub struct FileInput {
    /// Path to the file.
    pub file: String,
}

/// Input for unified search.
#[derive(Debug, Deserialize)]
pub struct SearchInput {
    /// One or more search queries.
    pub queries: Vec<String>,
    /// Optional additional directories to include in the ripgrep heatmap.
    /// Workspace roots are always searched; these paths are searched in addition.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// Input for codebase map.
#[derive(Debug, Deserialize)]
pub struct CodebaseMapInput {
    /// Subdirectory to start from (default: root)
    pub path: Option<String>,
    /// Max depth for traversal (default: 5)
    #[serde(default = "default_depth")]
    pub max_depth: usize,
    /// Whether to ask LSP for symbols (default: false)
    #[serde(default)]
    pub include_symbols: bool,
    /// Max lines of output before truncation (default: 2000)
    #[serde(default = "default_budget")]
    pub budget: usize,
    /// Symbol detail level: outline, signatures, or full (default: outline)
    #[serde(default = "default_detail_level")]
    pub detail_level: DetailLevel,
}

const fn default_depth() -> usize {
    5
}

const fn default_budget() -> usize {
    2000
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
            let ready = clients
                .get(lang)
                .is_some_and(|c| self.runtime.block_on(async { c.lock().await.is_ready() }));

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

        // Recovery: previously offline, now alive
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

        // Offline: newly dead, not yet reported
        let newly_dead: Vec<String> = dead
            .iter()
            .filter(|lang| notified.insert((*lang).clone()))
            .cloned()
            .collect();

        if !newly_dead.is_empty() {
            let langs = newly_dead.join(", ");
            parts.push(format!(
                "Language server{} offline: {langs} \u{2014} \
                 diagnostics unavailable for {langs} files. \
                 search and list_directory still work but without \
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
    async fn ensure_document_open(
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

    fn handle_document_symbols(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input: FileInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let path = Self::resolve_path(&input.file)?;

        debug!("Document symbols request: {}", input.file);

        let result = self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(&path).await?;

            if !client_mutex.lock().await.wait_ready().await {
                return Err(anyhow!("LSP server stopped responding during analysis"));
            }

            let params = DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            };
            client_mutex.lock().await.document_symbols(params).await
        })?;

        result.map_or_else(
            || Ok(CallToolResult::text("No symbols found")),
            |response| Ok(CallToolResult::text(format_document_symbols(&response))),
        )
    }

    /// Unified search: LSP workspace symbols with enrichment + ripgrep text matches.
    #[allow(clippy::too_many_lines, reason = "Core search orchestration")]
    fn handle_search(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        let input: SearchInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        if input.queries.is_empty() {
            return Err(anyhow!("queries must contain at least one search term"));
        }

        let extra_paths: Vec<PathBuf> = input
            .paths
            .iter()
            .filter_map(|p| {
                Self::resolve_path(p)
                    .ok()
                    .and_then(|resolved| match resolved.canonicalize() {
                        Ok(canonical) if canonical.is_dir() => Some(canonical),
                        _ => {
                            debug!("Skipping non-existent or non-directory search path: {p}");
                            None
                        }
                    })
            })
            .collect();

        let mut sections = Vec::new();

        for query in &input.queries {
            sections.push(self.search_single(query, &extra_paths));
        }

        Ok(CallToolResult::text(sections.join("\n")))
    }

    /// Executes a single search query: enriched LSP symbols + references + ripgrep.
    #[allow(
        clippy::too_many_lines,
        reason = "Core search logic with LSP enrichment"
    )]
    fn search_single(&self, query: &str, extra_paths: &[PathBuf]) -> String {
        use std::fmt::Write;

        debug!("Search request: query={query}");

        let roots = self.runtime.block_on(self.client_manager.roots());

        // 1. Collect raw SymbolInformation from all active LSP servers
        let symbols = self.runtime.block_on(async {
            let params = WorkspaceSymbolParams {
                query: query.to_string(),
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            };

            let clients = self.client_manager.active_clients().await;
            let mut all_symbols: Vec<SymbolInformation> = Vec::new();

            for client_mutex in clients.values() {
                if let Ok(Some(response)) = client_mutex
                    .lock()
                    .await
                    .workspace_symbols(params.clone())
                    .await
                {
                    collect_symbol_information(&response, &mut all_symbols);
                }
            }

            all_symbols
        });

        // 2. Enrich each symbol and collect references
        let mut symbol_sections = Vec::new();
        let mut all_ref_lines: HashMap<String, HashSet<u32>> = HashMap::new();

        for sym in &symbols {
            let enrichment = self.enrich_symbol(sym);

            // Collect reference lines for deduplication
            for (file, lines) in &enrichment.ref_lines {
                all_ref_lines.entry(file.clone()).or_default().extend(lines);
            }

            symbol_sections.push(format_enriched_symbol(sym, &enrichment, &roots));
        }

        // 3. Format References tier from collected reference locations
        let references_formatted = self.runtime.block_on(async {
            if symbols.len() <= 1 {
                // Single symbol: flat path, zero extra LSP calls
                let mut ref_by_file: BTreeMap<String, Vec<(u32, bool)>> = BTreeMap::new();

                for sym in &symbols {
                    let path = PathBuf::from(sym.location.uri.path().as_str());
                    let position = sym.location.range.start;
                    let def_file = sym.location.uri.path().to_string();
                    let def_line = sym.location.range.start.line + 1;

                    let refs = self.fetch_references(&path, position).await;
                    for loc in refs {
                        let file = loc.uri.path().to_string();
                        let line = loc.range.start.line + 1;
                        let is_def = file == def_file && line == def_line;
                        ref_by_file.entry(file).or_default().push((line, is_def));
                    }
                }

                format_clustered_references(&ref_by_file, &roots)
            } else {
                // Multiple symbols: collect refs and potentially disambiguate
                let mut refs_per_sym: Vec<Vec<Location>> = Vec::new();
                let mut total_refs = 0;

                for sym in &symbols {
                    let path = PathBuf::from(sym.location.uri.path().as_str());
                    let position = sym.location.range.start;
                    let refs = self.fetch_references(&path, position).await;
                    total_refs += refs.len();
                    refs_per_sym.push(refs);
                }

                let def_locations: Vec<(String, u32)> = symbols
                    .iter()
                    .map(|s| {
                        (
                            s.location.uri.path().to_string(),
                            s.location.range.start.line,
                        )
                    })
                    .collect();

                if total_refs > DISAMBIGUATION_REF_LIMIT {
                    // Too many refs — render flat
                    let mut ref_by_file: BTreeMap<String, Vec<(u32, bool)>> = BTreeMap::new();
                    for (sym_idx, refs) in refs_per_sym.iter().enumerate() {
                        let def_file = &def_locations[sym_idx].0;
                        let def_line = def_locations[sym_idx].1 + 1;
                        for loc in refs {
                            let file = loc.uri.path().to_string();
                            let line = loc.range.start.line + 1;
                            let is_def = file == *def_file && line == def_line;
                            ref_by_file.entry(file).or_default().push((line, is_def));
                        }
                    }
                    format_clustered_references(&ref_by_file, &roots)
                } else {
                    // Disambiguate: dedup refs, call definition for each, group by symbol
                    let mut unique_refs: Vec<Location> = Vec::new();
                    let mut seen: HashSet<(String, u32)> = HashSet::new();
                    for refs in &refs_per_sym {
                        for loc in refs {
                            let key = (loc.uri.path().to_string(), loc.range.start.line);
                            if seen.insert(key) {
                                unique_refs.push(loc.clone());
                            }
                        }
                    }

                    let mut groups: Vec<BTreeMap<String, Vec<(u32, bool)>>> =
                        vec![BTreeMap::new(); symbols.len()];
                    let mut fallback: BTreeMap<String, Vec<(u32, bool)>> = BTreeMap::new();

                    for loc in &unique_refs {
                        let ref_file = loc.uri.path().to_string();
                        let ref_line = loc.range.start.line + 1;
                        let is_def = def_locations
                            .iter()
                            .any(|(f, l)| f == &ref_file && *l == loc.range.start.line);

                        let ref_path = PathBuf::from(loc.uri.path().as_str());
                        let def_result = self.fetch_definition(&ref_path, loc.range.start).await;

                        let mut matched = false;
                        if let Some(def_loc) = def_result {
                            let resolved_file = def_loc.uri.path().to_string();
                            let resolved_line = def_loc.range.start.line;

                            for (i, (known_file, known_line)) in def_locations.iter().enumerate() {
                                if resolved_file == *known_file && resolved_line == *known_line {
                                    groups[i]
                                        .entry(ref_file.clone())
                                        .or_default()
                                        .push((ref_line, is_def));
                                    matched = true;
                                    break;
                                }
                            }
                        }

                        if !matched {
                            fallback
                                .entry(ref_file)
                                .or_default()
                                .push((ref_line, is_def));
                        }
                    }

                    format_disambiguated_references(&symbols, &groups, &fallback, &roots)
                }
            }
        });

        // 4. Ripgrep text matches with reference deduplication
        let mut search_dirs = roots;
        for path in extra_paths {
            if !search_dirs.contains(path) {
                search_dirs.push(path.clone());
            }
        }
        let rg_lines = Self::ripgrep_lines(query, &search_dirs);
        let file_matches = format_clustered_file_matches(&rg_lines, &all_ref_lines, &search_dirs);

        // 5. Combine tiers
        let has_symbols = !symbol_sections.is_empty();
        let has_references = !references_formatted.is_empty();
        let has_file_matches = !file_matches.is_empty();

        if !has_symbols && !has_references && !has_file_matches {
            return "No results found".to_string();
        }

        let mut output = String::new();

        if has_symbols {
            let _ = writeln!(output, "## Symbols\n");
            output.push_str(&symbol_sections.join("\n"));
        }

        if has_references {
            if has_symbols {
                output.push_str("\n\n");
            }
            let _ = writeln!(output, "## References\n");
            output.push_str(&references_formatted);
        }

        if has_file_matches {
            if has_symbols || has_references {
                output.push_str("\n\n");
            }
            let _ = writeln!(output, "## File matches\n");
            output.push_str(&file_matches);
        }

        output
    }

    /// Fetches references for a symbol position, returning empty vec on failure.
    async fn fetch_references(&self, path: &Path, position: Position) -> Vec<Location> {
        let Ok((uri, client_mutex)) = self.ensure_document_open(path).await else {
            return Vec::new();
        };

        let client = client_mutex.lock().await;

        if client.capabilities().references_provider.is_none() {
            return Vec::new();
        }

        let params = ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position,
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration: true,
            },
        };

        client
            .references(params)
            .await
            .unwrap_or(None)
            .unwrap_or_default()
    }

    /// Fetches the definition for a position, returning the first location on success.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "Client lock held across definition call"
    )]
    async fn fetch_definition(&self, path: &Path, position: Position) -> Option<Location> {
        let Ok((uri, client_mutex)) = self.ensure_document_open(path).await else {
            return None;
        };

        let client = client_mutex.lock().await;
        client.capabilities().definition_provider.as_ref()?;

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position,
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        };

        let response = client.definition(params).await.ok()??;
        extract_locations_from_definition(&response)
            .into_iter()
            .next()
    }

    /// Enriches a symbol with hover, call hierarchy, type hierarchy, and implementations.
    fn enrich_symbol(&self, sym: &SymbolInformation) -> SymbolEnrichment {
        let path = PathBuf::from(sym.location.uri.path().as_str());
        let position = sym.location.range.start;

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

            // Note: definition() is not called here because workspace/symbol
            // already returns symbols at their definition sites. Calling
            // definition() on a definition site returns itself, which we'd
            // suppress anyway. Definition enrichment would be useful when
            // enriching symbols found at *use* sites (e.g., ripgrep hits).

            // Type definition
            if caps.type_definition_provider.is_some() {
                let params = GotoDefinitionParams {
                    text_document_position_params: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier { uri: uri.clone() },
                        position,
                    },
                    work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                    partial_result_params: lsp_types::PartialResultParams::default(),
                };
                if let Ok(Some(response)) = client.type_definition(params).await {
                    let locations = extract_locations_from_definition(&response);
                    if let Some(loc) = locations.into_iter().next() {
                        enrichment.type_definition = Some(loc);
                    }
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
            match sym.kind {
                SymbolKind::FUNCTION | SymbolKind::METHOD | SymbolKind::CONSTRUCTOR => {
                    self.enrich_call_hierarchy(&client, &uri, position, &mut enrichment)
                        .await;
                }
                SymbolKind::STRUCT | SymbolKind::CLASS | SymbolKind::ENUM => {
                    self.enrich_implementations(&client, &uri, position, &mut enrichment)
                        .await;
                    self.enrich_type_hierarchy(&client, &uri, position, &mut enrichment)
                        .await;
                }
                SymbolKind::INTERFACE => {
                    self.enrich_implementations(&client, &uri, position, &mut enrichment)
                        .await;
                    self.enrich_subtypes(&client, &uri, position, &mut enrichment)
                        .await;
                }
                _ => {}
            }

            enrichment
        })
    }

    /// Fetches call hierarchy (incoming + outgoing) if the server supports it.
    async fn enrich_call_hierarchy(
        &self,
        client: &LspClient,
        uri: &lsp_types::Uri,
        position: Position,
        enrichment: &mut SymbolEnrichment,
    ) {
        if client.capabilities().call_hierarchy_provider.is_none() {
            return;
        }

        let prepare_params = CallHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
        };

        let Ok(Some(items)) = client.prepare_call_hierarchy(prepare_params).await else {
            return;
        };

        let Some(item) = items.into_iter().next() else {
            return;
        };

        // Incoming calls
        let incoming_params = CallHierarchyIncomingCallsParams {
            item: item.clone(),
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        };
        if let Ok(Some(calls)) = client.incoming_calls(incoming_params).await {
            enrichment.incoming_calls = calls;
        }

        // Outgoing calls
        let outgoing_params = CallHierarchyOutgoingCallsParams {
            item,
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        };
        if let Ok(Some(calls)) = client.outgoing_calls(outgoing_params).await {
            enrichment.outgoing_calls = calls;
        }
    }

    /// Fetches implementations if the server supports it.
    async fn enrich_implementations(
        &self,
        client: &LspClient,
        uri: &lsp_types::Uri,
        position: Position,
        enrichment: &mut SymbolEnrichment,
    ) {
        if client.capabilities().implementation_provider.is_none() {
            return;
        }

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        };

        if let Ok(Some(response)) = client.implementation(params).await {
            enrichment.implementations = extract_locations_from_definition(&response);
        }
    }

    /// Fetches full type hierarchy (supertypes + subtypes) if the server supports it.
    async fn enrich_type_hierarchy(
        &self,
        client: &LspClient,
        uri: &lsp_types::Uri,
        position: Position,
        enrichment: &mut SymbolEnrichment,
    ) {
        // type_hierarchy_provider not in lsp_types::ServerCapabilities;
        // attempt the call and gracefully handle errors.
        let prepare_params = TypeHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
        };

        let Ok(Some(items)) = client.prepare_type_hierarchy(prepare_params).await else {
            return;
        };

        let Some(item) = items.into_iter().next() else {
            return;
        };

        // Supertypes
        let super_params = TypeHierarchySupertypesParams {
            item: item.clone(),
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        };
        if let Ok(Some(types)) = client.supertypes(super_params).await {
            enrichment.supertypes = types;
        }

        // Subtypes
        let sub_params = TypeHierarchySubtypesParams {
            item,
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        };
        if let Ok(Some(types)) = client.subtypes(sub_params).await {
            enrichment.subtypes = types;
        }
    }

    /// Fetches subtypes only (for traits/interfaces).
    async fn enrich_subtypes(
        &self,
        client: &LspClient,
        uri: &lsp_types::Uri,
        position: Position,
        enrichment: &mut SymbolEnrichment,
    ) {
        // type_hierarchy_provider not in lsp_types::ServerCapabilities;
        // attempt the call and gracefully handle errors.
        let prepare_params = TypeHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
        };

        let Ok(Some(items)) = client.prepare_type_hierarchy(prepare_params).await else {
            return;
        };

        let Some(item) = items.into_iter().next() else {
            return;
        };

        let sub_params = TypeHierarchySubtypesParams {
            item,
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        };
        if let Ok(Some(types)) = client.subtypes(sub_params).await {
            enrichment.subtypes = types;
        }
    }

    /// Runs ripgrep and returns per-file line numbers.
    fn ripgrep_lines(query: &str, roots: &[PathBuf]) -> BTreeMap<String, Vec<u32>> {
        use std::process::Command;

        let mut cmd = Command::new("rg");
        cmd.args(["--line-number", "--no-heading", "--ignore-case", query]);

        for root in roots {
            cmd.arg(root);
        }

        let Ok(rg_output) = cmd.output() else {
            return BTreeMap::new();
        };

        if !rg_output.status.success() && rg_output.stdout.is_empty() {
            return BTreeMap::new();
        }

        let mut file_lines: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        let stdout = String::from_utf8_lossy(&rg_output.stdout);

        for line in stdout.lines() {
            let Some((file, rest)) = line.split_once(':') else {
                continue;
            };
            let Some((line_str, _content)) = rest.split_once(':') else {
                continue;
            };
            let Ok(line_num) = line_str.parse::<u32>() else {
                continue;
            };

            file_lines
                .entry(file.to_string())
                .or_default()
                .push(line_num);
        }

        file_lines
    }

    fn handle_diagnostics(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        let input: FileInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let path = Self::resolve_path(&input.file)?;

        debug!("Diagnostics request: {}", input.file);

        let diagnostics = self.runtime.block_on(async {
            let client_mutex = self.get_client_for_path(&path).await?;
            let mut doc_manager = self.doc_manager.lock().await;
            let client = client_mutex.lock().await;

            if !client.is_alive() {
                return Ok(Vec::new());
            }

            let uri = doc_manager.uri_for_path(&path)?;

            if let Some(notification) = doc_manager.ensure_open(&path).await? {
                // Snapshot generation *before* sending the change
                let snapshot = client.diagnostics_generation(&uri).await;

                match notification {
                    super::DocumentNotification::Open(params) => {
                        client.did_open(params).await?;
                    }
                    super::DocumentNotification::Change(params) => {
                        client.did_change(params).await?;
                    }
                }

                // Trigger flycheck on servers that only run diagnostics on save
                if client.wants_did_save() {
                    client.did_save(uri.clone()).await?;
                }

                drop(doc_manager);

                if client.wait_for_diagnostics_update(&uri, snapshot).await
                    == DiagnosticsWaitResult::Nothing
                {
                    return Ok(Vec::new());
                }
            } else {
                drop(doc_manager);
            }

            Ok::<_, anyhow::Error>(client.get_diagnostics(&uri).await)
        })?;

        if diagnostics.is_empty() {
            Ok(CallToolResult::text("No diagnostics"))
        } else {
            Ok(CallToolResult::text(format_diagnostics(&diagnostics)))
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Complexity of codebase map generation requires many lines"
    )]
    fn handle_codebase_map(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        use std::fmt::Write;
        struct MapEntry {
            path: PathBuf,
            depth: usize,
            is_dir: bool,
            symbols: Option<String>,
            display_name: Option<String>,
        }
        let input: CodebaseMapInput =
            serde_json::from_value(arguments.unwrap_or_else(|| serde_json::json!({})))
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let root_paths: Vec<PathBuf> = if let Some(p) = &input.path {
            vec![Self::resolve_path(p)?]
        } else {
            let roots = self.runtime.block_on(self.client_manager.roots());
            if roots.is_empty() {
                vec![std::env::current_dir()?]
            } else {
                roots
            }
        };
        let multi_root = root_paths.len() > 1;

        debug!(
            "Codebase map request: paths={:?} depth={} symbols={}",
            root_paths, input.max_depth, input.include_symbols
        );

        // 1. Walk Directory and collect entries
        let mut entries = Vec::new();

        for root_path in &root_paths {
            // For multi-root, add the root itself as a top-level directory entry
            let root_prefix = if multi_root {
                root_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            } else {
                None
            };

            let walker = WalkBuilder::new(root_path)
                .max_depth(Some(input.max_depth))
                .git_ignore(true)
                .hidden(true)
                .build();

            // Add a virtual root entry for multi-root display
            if let Some(ref name) = root_prefix {
                entries.push(MapEntry {
                    path: root_path.clone(),
                    depth: 1,
                    is_dir: true,
                    symbols: None,
                    display_name: Some(format!("{name}/")),
                });
            }

            for result in walker {
                match result {
                    Ok(entry) => {
                        let path = entry.path();
                        if path == root_path {
                            continue;
                        } // Skip root itself

                        let rel_path = path.strip_prefix(root_path).unwrap_or(path);
                        let depth = rel_path.components().count();
                        let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());

                        // In multi-root mode, add 1 to depth for nesting under root name
                        let adjusted_depth = if multi_root { depth + 1 } else { depth };

                        entries.push(MapEntry {
                            path: path.to_path_buf(),
                            depth: adjusted_depth,
                            is_dir,
                            symbols: None,
                            display_name: None,
                        });
                    }
                    Err(err) => warn!("Error walking directory: {}", err),
                }
            }
        }

        // Pick the first root for relative path display in single-root mode
        let primary_root = root_paths.first().cloned().unwrap_or_default();

        // 2. Fetch Symbols (Async Phase)
        let unavailable_langs = if input.include_symbols {
            let entries_len = entries.len();
            let detail_level = input.detail_level;
            debug!("Fetching symbols for {} files", entries_len);

            self.runtime.block_on(async {
                let mut unavailable: Vec<String> = Vec::new();

                for entry in &mut entries {
                    if entry.is_dir {
                        continue;
                    }

                    // Simple extension check to avoid wasted LSP calls
                    let lang_id = {
                        let doc_manager = self.doc_manager.lock().await;
                        doc_manager.language_id_for_path(&entry.path).to_string()
                    };

                    if let Ok(client_mutex) = self
                        .client_manager
                        .get_client_for_path(&entry.path, &lang_id)
                        .await
                    {
                        if let Ok((uri, _)) = self.ensure_document_open(&entry.path).await {
                            let params = DocumentSymbolParams {
                                text_document: TextDocumentIdentifier { uri },
                                work_done_progress_params:
                                    lsp_types::WorkDoneProgressParams::default(),
                                partial_result_params: lsp_types::PartialResultParams::default(),
                            };

                            let client = client_mutex.lock().await;
                            let result = client.document_symbols(params).await;
                            drop(client);

                            if let Ok(Some(response)) = result {
                                entry.symbols =
                                    Some(format_compact_symbols(&response, detail_level));
                            }
                        }
                    } else if !unavailable.contains(&lang_id) {
                        warn!("[{lang_id}] unavailable during codebase map symbol fetch");
                        unavailable.push(lang_id);
                    }
                }

                unavailable
            })
        } else {
            Vec::new()
        };

        // 3. Render Output
        let mut output = String::new();
        let mut line_count = 0;
        let budget = input.budget;

        for entry in entries {
            if line_count >= budget {
                output.push_str("... (truncated)\n");
                break;
            }

            // Indentation
            let indent = "  ".repeat(entry.depth - 1);

            let display = if let Some(ref name) = entry.display_name {
                name.clone()
            } else {
                // Find the matching root for this entry to compute relative path
                let matching_root = root_paths
                    .iter()
                    .find(|r| entry.path.starts_with(r))
                    .unwrap_or(&primary_root);
                let rel_path = entry
                    .path
                    .strip_prefix(matching_root)
                    .unwrap_or(&entry.path);
                let name = rel_path.file_name().unwrap_or_default().to_string_lossy();
                let marker = if entry.is_dir { "/" } else { "" };
                format!("{name}{marker}")
            };

            let _ = writeln!(output, "{indent}{display}");
            line_count += 1;

            if let Some(symbols) = &entry.symbols {
                let sym_indent = "  ".repeat(entry.depth);
                for line in symbols.lines() {
                    if line_count >= budget {
                        break;
                    }
                    // Truncate long symbol lines
                    let max_width = 120;
                    let display_line = if line.len() > max_width {
                        format!("{}...", &line[..max_width])
                    } else {
                        line.to_string()
                    };

                    let _ = writeln!(output, "{sym_indent}{display_line}");
                    line_count += 1;
                }
            }
        }

        for lang in &unavailable_langs {
            let _ = writeln!(
                output,
                "\nWarning: [{lang}] unavailable, symbols may be incomplete"
            );
        }

        Ok(CallToolResult::text(output))
    }
}

impl ToolHandler for LspBridgeHandler {
    fn list_tools(&self) -> Vec<Tool> {
        vec![
            Tool {
                name: "search".to_string(),
                description: Some("Search for a symbol or pattern across the workspace. Returns LSP workspace symbols (semantic) plus a file heatmap showing which files contain the query and where (match count + line range).".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "queries": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Symbol names or text patterns to search for"
                        },
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Additional directories to include in the text search (heatmap only — no LSP symbol resolution). Workspace roots are always searched."
                        }
                    },
                    "required": ["queries"]
                }),
            },
            Tool {
                name: "document_symbols".to_string(),
                description: Some("Get the symbol outline of a file (functions, classes, variables, etc.).".to_string()),
                input_schema: file_schema(),
            },
            Tool {
                name: "diagnostics".to_string(),
                description: Some("Get diagnostics (errors, warnings, hints) for a file.".to_string()),
                input_schema: file_schema(),
            },
            Tool {
                name: "codebase_map".to_string(),
                description: Some("Generate a high-level file tree of the project, optionally including symbols from LSP.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Subdirectory to map (default: project root)" },
                        "max_depth": { "type": "integer", "description": "Max depth for traversal (default: 5)" },
                        "include_symbols": { "type": "boolean", "description": "Ask LSP for symbols in files (default: false)" },
                        "budget": { "type": "integer", "description": "Max lines of output (default: 2000)" },
                        "detail_level": {
                            "type": "string",
                            "enum": ["outline", "signatures", "full"],
                            "description": "Symbol detail: outline (classes/structs only), signatures (+functions/methods), full (everything). Default: outline"
                        }
                    },
                    "required": []
                }),
            },
            Tool {
                name: "list_directory".to_string(),
                description: Some("List the contents of a directory. Shows directories, files with sizes, and symlinks with targets. Symlinks are not followed. Path must be within workspace roots.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Absolute or relative path to the directory" }
                    },
                    "required": ["path"]
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
        let file_path = Self::extract_file_path(arguments.as_ref());
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
        let health = if METHODS_SKIP_WAIT.contains(&name) {
            ServerHealth {
                dead: vec![],
                notification: None,
            }
        } else if let Some(ref path) = file_path {
            // File-scoped: wait for the specific server
            self.runtime.block_on(self.wait_for_server_ready(path));
            let touched: Vec<String> = self
                .runtime
                .block_on(self.language_for_path(path))
                .into_iter()
                .collect();
            self.check_server_health(&touched)
        } else {
            // Symbol-only: wait for all servers
            self.runtime.block_on(self.wait_for_all_servers_ready());
            let touched: Vec<String> = self
                .runtime
                .block_on(self.client_manager.active_clients())
                .keys()
                .cloned()
                .collect();
            self.check_server_health(&touched)
        };

        // File-scoped tool with dead server: skip dispatch, return notification
        if !health.dead.is_empty() && file_path.is_some() {
            broadcast_result(true);
            return Ok(CallToolResult::text(
                health.notification.unwrap_or_default(),
            ));
        }

        // Dispatch tool
        let mut result = match name {
            "search" => self.handle_search(arguments),
            "document_symbols" => self.handle_document_symbols(arguments),
            "diagnostics" => self.handle_diagnostics(arguments),
            "codebase_map" => self.handle_codebase_map(arguments),
            "list_directory" => self.handle_list_directory(arguments),
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

// ... (existing schema helpers)

fn format_compact_symbols(response: &DocumentSymbolResponse, level: DetailLevel) -> String {
    let mut result = Vec::new();
    match response {
        DocumentSymbolResponse::Flat(symbols) => {
            for sym in symbols {
                if matches_detail_level(sym.kind, level) {
                    result.push(format!("{} {:?}", sym.name, sym.kind));
                }
            }
        }
        DocumentSymbolResponse::Nested(symbols) => {
            for sym in symbols {
                if matches_detail_level(sym.kind, level) {
                    result.push(format!("{} {:?}", sym.name, sym.kind));
                }
            }
        }
    }
    result.join("\n")
}

const fn matches_detail_level(kind: lsp_types::SymbolKind, level: DetailLevel) -> bool {
    use lsp_types::SymbolKind;

    // Outline: structural types + document structure (STRING for markdown headings, KEY for YAML/JSON)
    let is_outline = matches!(
        kind,
        SymbolKind::FILE
            | SymbolKind::MODULE
            | SymbolKind::NAMESPACE
            | SymbolKind::PACKAGE
            | SymbolKind::CLASS
            | SymbolKind::INTERFACE
            | SymbolKind::ENUM
            | SymbolKind::STRUCT
            | SymbolKind::STRING
            | SymbolKind::KEY
    );

    // Signatures: outline + callable members
    let is_signature = matches!(
        kind,
        SymbolKind::FUNCTION
            | SymbolKind::METHOD
            | SymbolKind::CONSTRUCTOR
            | SymbolKind::PROPERTY
            | SymbolKind::EVENT
    );

    match level {
        DetailLevel::Outline => is_outline,
        DetailLevel::Signatures => is_outline || is_signature,
        DetailLevel::Full => true,
    }
}

fn file_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file": { "type": "string", "description": "Absolute path to the file" }
        },
        "required": ["file"]
    })
}

fn format_location(location: &Location) -> String {
    let path = location.uri.path();
    let line = location.range.start.line + 1;
    let col = location.range.start.character + 1;
    format!("{path}:{line}:{col}")
}

fn format_document_symbols(response: &DocumentSymbolResponse) -> String {
    match response {
        DocumentSymbolResponse::Flat(symbols) => symbols
            .iter()
            .map(format_symbol_info)
            .collect::<Vec<_>>()
            .join("\n"),
        DocumentSymbolResponse::Nested(symbols) => format_nested_symbols(symbols, 0),
    }
}

fn format_symbol_info(sym: &SymbolInformation) -> String {
    let kind = format!("{:?}", sym.kind);
    let loc = format_location(&sym.location);
    format!("{} [{}] {}", sym.name, kind, loc)
}

fn format_nested_symbols(symbols: &[DocumentSymbol], indent: usize) -> String {
    let mut result = Vec::new();
    for sym in symbols {
        let kind = format!("{:?}", sym.kind);
        let prefix = "  ".repeat(indent);
        let line = sym.range.start.line + 1;
        result.push(format!("{}{} [{}] line {}", prefix, sym.name, kind, line));
        if let Some(children) = &sym.children {
            result.push(format_nested_symbols(children, indent + 1));
        }
    }
    result.join("\n")
}

fn format_diagnostics(diagnostics: &[Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(|d| {
            let severity = match d.severity {
                Some(DiagnosticSeverity::ERROR) => "error",
                Some(DiagnosticSeverity::WARNING) => "warning",
                Some(DiagnosticSeverity::INFORMATION) => "info",
                Some(DiagnosticSeverity::HINT) => "hint",
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
                format!("{}:{}: [{}] {}: {}", line, col, severity, source, d.message)
            } else {
                format!(
                    "{}:{}: [{}] {}({}): {}",
                    line, col, severity, source, code, d.message
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ─── Search enrichment types and formatting ─────────────────────────────

/// Enrichment data collected for a single workspace symbol.
#[derive(Default)]
struct SymbolEnrichment {
    /// Hover content (signature + docs).
    hover: Option<String>,
    /// Type definition location.
    type_definition: Option<Location>,
    /// Reference line numbers per file (for deduplication).
    ref_lines: HashMap<String, HashSet<u32>>,
    /// Incoming calls (who calls this function).
    incoming_calls: Vec<CallHierarchyIncomingCall>,
    /// Outgoing calls (what this function calls).
    outgoing_calls: Vec<CallHierarchyOutgoingCall>,
    /// Implementation locations (methods for structs, implementors for traits).
    implementations: Vec<Location>,
    /// Supertypes in the type hierarchy.
    supertypes: Vec<TypeHierarchyItem>,
    /// Subtypes in the type hierarchy.
    subtypes: Vec<TypeHierarchyItem>,
}

/// Collects `SymbolInformation` from a `WorkspaceSymbolResponse`.
fn collect_symbol_information(
    response: &WorkspaceSymbolResponse,
    out: &mut Vec<SymbolInformation>,
) {
    match response {
        WorkspaceSymbolResponse::Flat(symbols) => out.extend(symbols.iter().cloned()),
        WorkspaceSymbolResponse::Nested(symbols) => {
            for s in symbols {
                if let lsp_types::OneOf::Left(location) = &s.location {
                    #[allow(deprecated, reason = "LSP spec uses deprecated fields")]
                    out.push(SymbolInformation {
                        name: s.name.clone(),
                        kind: s.kind,
                        tags: s.tags.clone(),
                        deprecated: None,
                        location: location.clone(),
                        container_name: s.container_name.clone(),
                    });
                }
            }
        }
    }
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

/// Extracts locations from a `GotoDefinitionResponse`.
fn extract_locations_from_definition(response: &GotoDefinitionResponse) -> Vec<Location> {
    match response {
        GotoDefinitionResponse::Scalar(loc) => vec![loc.clone()],
        GotoDefinitionResponse::Array(locs) => locs.clone(),
        GotoDefinitionResponse::Link(links) => links
            .iter()
            .map(|link| Location {
                uri: link.target_uri.clone(),
                range: link.target_selection_range,
            })
            .collect(),
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

/// Formats an enriched symbol for the Symbols tier.
fn format_enriched_symbol(
    sym: &SymbolInformation,
    enrichment: &SymbolEnrichment,
    roots: &[PathBuf],
) -> String {
    use std::fmt::Write;

    let kind = format!("{:?}", sym.kind);
    let path = display_path(sym.location.uri.path().as_str(), roots);
    let line = sym.location.range.start.line + 1;

    let mut out = format!("{} [{kind}] {path}:{line}", sym.name);

    // Hover content (indented)
    if let Some(hover) = &enrichment.hover {
        for hover_line in hover.lines() {
            let _ = write!(out, "\n  {hover_line}");
        }
    }

    // Type definition
    if let Some(td) = &enrichment.type_definition {
        let td_path = display_path(td.uri.path().as_str(), roots);
        let td_line = td.range.start.line + 1;
        let _ = write!(out, "\n  Type: {td_path}:{td_line}");
    }

    // Call hierarchy (functions/methods)
    if !enrichment.incoming_calls.is_empty() {
        out.push_str("\n\n  Called by:");
        for call in &enrichment.incoming_calls {
            let call_path = display_path(call.from.uri.path().as_str(), roots);
            let call_line = call.from.range.start.line + 1;
            let _ = write!(out, "\n    {}  {call_path}:{call_line}", call.from.name);
        }
    }

    if !enrichment.outgoing_calls.is_empty() {
        out.push_str("\n\n  Calls:");
        for call in &enrichment.outgoing_calls {
            let call_path = display_path(call.to.uri.path().as_str(), roots);
            let call_line = call.to.range.start.line + 1;
            let _ = write!(out, "\n    {}  {call_path}:{call_line}", call.to.name);
        }
    }

    // Implementations (structs/traits)
    if !enrichment.implementations.is_empty() {
        out.push_str("\n\n  Implementations:");
        for loc in &enrichment.implementations {
            let impl_path = display_path(loc.uri.path().as_str(), roots);
            let impl_line = loc.range.start.line + 1;
            let _ = write!(out, "\n    {impl_path}:{impl_line}");
        }
    }

    // Type hierarchy
    if !enrichment.supertypes.is_empty() {
        out.push_str("\n\n  Supertypes:");
        for item in &enrichment.supertypes {
            let item_path = display_path(item.uri.path().as_str(), roots);
            let item_line = item.range.start.line + 1;
            let _ = write!(
                out,
                "\n    {} [{:?}]  {item_path}:{item_line}",
                item.name, item.kind
            );
        }
    }

    if !enrichment.subtypes.is_empty() {
        out.push_str("\n\n  Subtypes:");
        for item in &enrichment.subtypes {
            let item_path = display_path(item.uri.path().as_str(), roots);
            let item_line = item.range.start.line + 1;
            let _ = write!(
                out,
                "\n    {} [{:?}]  {item_path}:{item_line}",
                item.name, item.kind
            );
        }
    }

    out
}

/// Gap-based clustering: groups sorted line numbers into clusters.
///
/// Merge distance = `ceil(sqrt(max_line))`. Lines within the merge distance
/// are grouped together. Returns `Vec<(first_line, last_line, lines)>`.
fn cluster_lines(lines: &[u32]) -> Vec<(u32, u32, Vec<u32>)> {
    if lines.is_empty() {
        return Vec::new();
    }

    let mut sorted = lines.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let max_line = *sorted.last().unwrap_or(&1);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "Line numbers safely fit in f64 and truncated u32"
    )]
    let merge_distance = (f64::from(max_line).sqrt().ceil()) as u32;
    let merge_distance = merge_distance.max(1);

    let mut clusters: Vec<(u32, u32, Vec<u32>)> = Vec::new();
    let mut cluster_start = sorted[0];
    let mut cluster_end = sorted[0];
    let mut cluster_lines = vec![sorted[0]];

    for &line in &sorted[1..] {
        if line - cluster_end <= merge_distance {
            cluster_end = line;
            cluster_lines.push(line);
        } else {
            clusters.push((cluster_start, cluster_end, cluster_lines));
            cluster_start = line;
            cluster_end = line;
            cluster_lines = vec![line];
        }
    }
    clusters.push((cluster_start, cluster_end, cluster_lines));

    clusters
}

/// Formats a single cluster as `[start-end]: count (lines ...)` or `[line]: 1 (line N)`.
fn format_cluster(start: u32, end: u32, lines: &[u32]) -> String {
    let count = lines.len();
    let range = format!("[{start}-{end}]");
    let line_list = if count <= 5 {
        let nums: Vec<String> = lines.iter().map(ToString::to_string).collect();
        if count == 1 {
            format!("line {}", nums[0])
        } else {
            format!("lines {}", nums.join(", "))
        }
    } else {
        let first_three: Vec<String> = lines[..3].iter().map(ToString::to_string).collect();
        format!("lines {}, ... +{} more", first_three.join(", "), count - 3)
    };

    let match_word = if count == 1 { "match" } else { "matches" };
    format!("  {range}: {count} {match_word} ({line_list})")
}

/// Formats the References tier with gap-based clustering.
fn format_clustered_references(
    ref_by_file: &BTreeMap<String, Vec<(u32, bool)>>,
    roots: &[PathBuf],
) -> String {
    use std::fmt::Write;

    if ref_by_file.is_empty() {
        return String::new();
    }

    // Sort by total usage count descending
    let mut sorted: Vec<_> = ref_by_file.iter().collect();
    sorted.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));

    let mut output = String::new();

    for (file, entries) in sorted {
        let path = display_path(file, roots);
        let count = entries.len();
        let usage_word = if count == 1 { "usage" } else { "usages" };
        let _ = writeln!(output, "{path}: {count} {usage_word}");

        // Build line list with def markers
        let mut lines: Vec<u32> = entries.iter().map(|(l, _)| *l).collect();
        lines.sort_unstable();
        lines.dedup();

        let def_lines: HashSet<u32> = entries
            .iter()
            .filter(|(_, is_def)| *is_def)
            .map(|(l, _)| *l)
            .collect();

        let clusters = cluster_lines(&lines);
        for (start, end, cluster_lines) in &clusters {
            let mut cluster_str = format_cluster(*start, *end, cluster_lines);
            // Append [def] marker if any line in this cluster is the definition
            if cluster_lines.iter().any(|l| def_lines.contains(l)) {
                cluster_str.push_str(" [def]");
            }
            let _ = writeln!(output, "{cluster_str}");
        }
    }

    output.truncate(output.trim_end().len());
    output
}

/// Formats disambiguated references grouped by owning symbol.
fn format_disambiguated_references(
    symbols: &[SymbolInformation],
    groups: &[BTreeMap<String, Vec<(u32, bool)>>],
    fallback: &BTreeMap<String, Vec<(u32, bool)>>,
    roots: &[PathBuf],
) -> String {
    use std::fmt::Write;

    let mut output = String::new();

    for (i, (sym, group)) in symbols.iter().zip(groups).enumerate() {
        if group.is_empty() {
            continue;
        }

        let sym_path = display_path(sym.location.uri.path().as_str(), roots);
        let _ = writeln!(output, "{sym_path} {} ({}):", sym.name, i + 1);
        let group_formatted = format_clustered_references(group, roots);
        for line in group_formatted.lines() {
            let _ = writeln!(output, "  {line}");
        }
    }

    if !fallback.is_empty() {
        let _ = writeln!(output, "Unresolved:");
        let fallback_formatted = format_clustered_references(fallback, roots);
        for line in fallback_formatted.lines() {
            let _ = writeln!(output, "  {line}");
        }
    }

    output.truncate(output.trim_end().len());
    output
}

/// Formats the File matches tier with reference deduplication and clustering.
fn format_clustered_file_matches(
    rg_lines: &BTreeMap<String, Vec<u32>>,
    ref_lines: &HashMap<String, HashSet<u32>>,
    roots: &[PathBuf],
) -> String {
    use std::fmt::Write;

    if rg_lines.is_empty() {
        return String::new();
    }

    let mut output = String::new();

    // Sort by match count descending
    let mut sorted: Vec<_> = rg_lines.iter().collect();
    sorted.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));

    for (file, lines) in sorted {
        // Subtract reference lines
        let remaining: Vec<u32> = ref_lines.get(file.as_str()).map_or_else(
            || lines.clone(),
            |refs| {
                lines
                    .iter()
                    .copied()
                    .filter(|l| !refs.contains(l))
                    .collect()
            },
        );

        if remaining.is_empty() {
            continue;
        }

        let path = display_path(file, roots);
        let count = remaining.len();
        let match_word = if count == 1 { "match" } else { "matches" };
        let _ = writeln!(output, "{path}: {count} {match_word}");

        let clusters = cluster_lines(&remaining);
        for (start, end, cluster_lines) in &clusters {
            let _ = writeln!(output, "{}", format_cluster(*start, *end, cluster_lines));
        }
    }

    output.truncate(output.trim_end().len());
    output
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use lsp_types::{Range, SymbolInformation, SymbolKind, WorkspaceSymbolResponse};

    fn make_position(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn make_range(start_line: u32, start_char: u32, end_line: u32, end_char: u32) -> Range {
        Range {
            start: make_position(start_line, start_char),
            end: make_position(end_line, end_char),
        }
    }

    fn make_symbol_info(
        name: &str,
        kind: SymbolKind,
        uri: &str,
        line: u32,
    ) -> Result<SymbolInformation> {
        #[allow(
            deprecated,
            reason = "LSP spec uses deprecated fields in some versions"
        )]
        Ok(SymbolInformation {
            name: name.to_string(),
            kind,
            tags: None,
            deprecated: None,
            location: Location {
                uri: uri.parse()?,
                range: make_range(line, 0, line, 10),
            },
            container_name: None,
        })
    }

    // ─── cluster_lines tests ─────────────────────────────────────────────

    #[test]
    fn test_cluster_lines_empty() {
        assert!(cluster_lines(&[]).is_empty());
    }

    #[test]
    fn test_cluster_lines_single() {
        let clusters = cluster_lines(&[42]);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0], (42, 42, vec![42]));
    }

    #[test]
    fn test_cluster_lines_close_together() {
        let clusters = cluster_lines(&[10, 11, 12]);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].0, 10);
        assert_eq!(clusters[0].1, 12);
    }

    #[test]
    fn test_cluster_lines_far_apart() {
        // merge_distance = ceil(sqrt(1000)) = 32
        let clusters = cluster_lines(&[1, 1000]);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].2, vec![1]);
        assert_eq!(clusters[1].2, vec![1000]);
    }

    #[test]
    fn test_cluster_lines_dedup() {
        let clusters = cluster_lines(&[5, 5, 5]);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].2, vec![5]);
    }

    #[test]
    fn test_cluster_lines_unsorted_and_sorted() {
        // merge_distance = ceil(sqrt(30)) = 6, gaps of 10 → 3 clusters
        let clusters = cluster_lines(&[30, 10, 20]);
        assert_eq!(clusters.len(), 3);
        assert_eq!(clusters[0].2, vec![10]);
        assert_eq!(clusters[1].2, vec![20]);
        assert_eq!(clusters[2].2, vec![30]);

        // Lines within merge distance should cluster: sqrt(5) ≈ 3
        let clusters = cluster_lines(&[3, 1, 5]);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].2, vec![1, 3, 5]);
    }

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

    // ─── format_cluster tests ────────────────────────────────────────────

    #[test]
    fn test_format_cluster_single_line() {
        let result = format_cluster(42, 42, &[42]);
        assert!(result.contains("[42-42]"));
        assert!(result.contains("1 match"));
        assert!(result.contains("line 42"));
    }

    #[test]
    fn test_format_cluster_range() {
        let result = format_cluster(10, 20, &[10, 15, 20]);
        assert!(result.contains("[10-20]"));
        assert!(result.contains("3 matches"));
        assert!(result.contains("lines 10, 15, 20"));
    }

    #[test]
    fn test_format_cluster_many_lines_truncated() {
        let lines: Vec<u32> = (1..=10).collect();
        let result = format_cluster(1, 10, &lines);
        assert!(result.contains("10 matches"));
        assert!(result.contains("+7 more"));
    }

    // ─── collect_symbol_information tests ────────────────────────────────

    #[test]
    fn test_collect_symbol_information_flat() -> Result<()> {
        let sym = make_symbol_info("test_fn", SymbolKind::FUNCTION, "file:///test.rs", 0)?;
        let response = WorkspaceSymbolResponse::Flat(vec![sym]);
        let mut out = Vec::new();
        collect_symbol_information(&response, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "test_fn");
        Ok(())
    }

    #[test]
    fn test_collect_symbol_information_empty() {
        let response = WorkspaceSymbolResponse::Flat(vec![]);
        let mut out = Vec::new();
        collect_symbol_information(&response, &mut out);
        assert!(out.is_empty());
    }

    // ─── format_clustered_file_matches dedup tests ───────────────────────

    #[test]
    fn test_file_matches_dedup_removes_ref_lines() {
        let mut rg_lines = BTreeMap::new();
        rg_lines.insert("/src/lib.rs".to_string(), vec![10, 20, 30]);

        let mut ref_lines = HashMap::new();
        ref_lines.insert("/src/lib.rs".to_string(), HashSet::from([10, 30]));

        let roots = vec![PathBuf::from("/")];
        let result = format_clustered_file_matches(&rg_lines, &ref_lines, &roots);

        // Only line 20 should remain
        assert!(result.contains("1 match"));
        assert!(!result.contains("3 match"));
    }

    #[test]
    fn test_file_matches_dedup_all_removed() {
        let mut rg_lines = BTreeMap::new();
        rg_lines.insert("/src/lib.rs".to_string(), vec![10, 20]);

        let mut ref_lines = HashMap::new();
        ref_lines.insert("/src/lib.rs".to_string(), HashSet::from([10, 20]));

        let roots = vec![PathBuf::from("/")];
        let result = format_clustered_file_matches(&rg_lines, &ref_lines, &roots);

        // All lines deduped — file should be omitted
        assert!(result.is_empty());
    }

    // ─── format_enriched_symbol tests ────────────────────────────────────

    #[test]
    fn test_format_enriched_symbol_basic() -> Result<()> {
        let sym = make_symbol_info(
            "my_func",
            SymbolKind::FUNCTION,
            "file:///project/src/lib.rs",
            42,
        )?;
        let enrichment = SymbolEnrichment::default();
        let roots = vec![PathBuf::from("/project")];

        let output = format_enriched_symbol(&sym, &enrichment, &roots);
        assert!(output.contains("my_func"));
        assert!(output.contains("[Function]"));
        assert!(output.contains("src/lib.rs:43"));
        Ok(())
    }

    #[test]
    fn test_format_enriched_symbol_with_hover() -> Result<()> {
        let sym = make_symbol_info(
            "my_func",
            SymbolKind::FUNCTION,
            "file:///project/src/lib.rs",
            0,
        )?;
        let enrichment = SymbolEnrichment {
            hover: Some("pub fn my_func() -> bool".to_string()),
            ..Default::default()
        };
        let roots = vec![PathBuf::from("/project")];

        let output = format_enriched_symbol(&sym, &enrichment, &roots);
        assert!(output.contains("pub fn my_func() -> bool"));
        Ok(())
    }
}

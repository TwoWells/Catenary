/*
 * Copyright (C) 2026 Mark Wells Dev
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

//! Bridge handler that maps MCP tool calls to LSP requests.

use anyhow::{Result, anyhow};
use ignore::WalkBuilder;
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyOutgoingCall,
    CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams, CodeActionContext,
    CodeActionOrCommand, CodeActionParams, CompletionItem, CompletionParams, CompletionResponse,
    Diagnostic, DiagnosticSeverity, DocumentChanges, DocumentFormattingParams,
    DocumentRangeFormattingParams, DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse,
    FormattingOptions, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, Location,
    LocationLink, Position, Range, ReferenceContext, ReferenceParams, RenameParams, SignatureHelp,
    SignatureHelpParams, SymbolInformation, TextDocumentIdentifier, TextDocumentPositionParams,
    TextEdit, TypeHierarchyItem, TypeHierarchyPrepareParams, TypeHierarchySubtypesParams,
    TypeHierarchySupertypesParams, WorkspaceEdit, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::config::Config;
use crate::lsp::{ClientManager, LspClient, ServerState};
use crate::mcp::{CallToolResult, Tool, ToolHandler};
use crate::session::{EventBroadcaster, EventKind};

/// Methods that should wait for server readiness before executing.
const METHODS_WAIT_FOR_READY: &[&str] = &[
    "hover",
    "definition",
    "type_definition",
    "implementation",
    "find_references",
    "document_symbols",
    "code_actions",
    "completion",
    "diagnostics",
];

use super::{DocumentManager, DocumentNotification};

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

/// Input for tools that need file + position.
#[derive(Debug, Deserialize)]
pub struct PositionInput {
    /// Path to the file.
    pub file: String,
    /// 0-indexed line number.
    pub line: u32,
    /// 0-indexed character position.
    pub character: u32,
}

/// Input for tools that accept either a symbol name or file/line/character position.
#[derive(Debug, Deserialize)]
pub struct SymbolOrPositionInput {
    /// Symbol name to search for (uses workspace/document symbols).
    pub symbol: Option<String>,
    /// File path — required for position mode, optional for symbol mode (narrows search scope).
    pub file: Option<String>,
    /// 0-indexed line number — required if not using symbol.
    pub line: Option<u32>,
    /// 0-indexed character position — required if not using symbol.
    pub character: Option<u32>,
}

/// Input for tools that need only a file path.
#[derive(Debug, Deserialize)]
pub struct FileInput {
    /// Path to the file.
    pub file: String,
    /// Whether to wait for the LSP server to finish analysis before returning results.
    ///
    /// Defaults to `true`.
    #[serde(default = "default_true")]
    pub wait_for_reanalysis: bool,
}

const fn default_true() -> bool {
    true
}

/// Input for `find_references` - accepts either symbol name OR position.
#[derive(Debug, Deserialize)]
pub struct FindReferencesInput {
    /// Symbol name to search for (uses workspace symbols)
    pub symbol: Option<String>,
    /// File path (required if using position, optional if using symbol to narrow scope)
    pub file: Option<String>,
    /// Line number (0-indexed) - required if not using symbol
    pub line: Option<u32>,
    /// Character position (0-indexed) - required if not using symbol
    pub character: Option<u32>,
    #[serde(default = "default_true")]
    pub include_declaration: bool,
}

/// Input for unified search.
#[derive(Debug, Deserialize)]
pub struct SearchInput {
    pub query: String,
}

/// Input for code actions.
#[derive(Debug, Deserialize)]
pub struct CodeActionInput {
    pub file: String,
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
}

/// Input for rename.
#[derive(Debug, Deserialize)]
pub struct RenameInput {
    pub file: String,
    pub line: u32,
    pub character: u32,
    pub new_name: String,
}

/// Input for formatting.
#[derive(Debug, Deserialize)]
pub struct FormattingInput {
    pub file: String,
    #[serde(default = "default_tab_size")]
    pub tab_size: u32,
    #[serde(default)]
    pub insert_spaces: bool,
}

const fn default_tab_size() -> u32 {
    4
}

/// Input for range formatting.
#[derive(Debug, Deserialize)]
pub struct RangeFormattingInput {
    pub file: String,
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    #[serde(default = "default_tab_size")]
    pub tab_size: u32,
    #[serde(default)]
    pub insert_spaces: bool,
}

/// Input for call hierarchy.
#[derive(Debug, Deserialize)]
pub struct CallHierarchyInput {
    /// Symbol name to search for (uses workspace/document symbols).
    pub symbol: Option<String>,
    /// File path — required for position mode, optional for symbol mode.
    pub file: Option<String>,
    /// 0-indexed line number — required if not using symbol.
    pub line: Option<u32>,
    /// 0-indexed character position — required if not using symbol.
    pub character: Option<u32>,
    /// "incoming" or "outgoing"
    pub direction: String,
}

/// Input for type hierarchy.
#[derive(Debug, Deserialize)]
pub struct TypeHierarchyInput {
    /// Symbol name to search for (uses workspace/document symbols).
    pub symbol: Option<String>,
    /// File path — required for position mode, optional for symbol mode.
    pub file: Option<String>,
    /// 0-indexed line number — required if not using symbol.
    pub line: Option<u32>,
    /// 0-indexed character position — required if not using symbol.
    pub character: Option<u32>,
    /// "supertypes" or "subtypes"
    pub direction: String,
}

/// Input for auto-fixing.
#[derive(Debug, Deserialize)]
pub struct ApplyQuickFixInput {
    pub file: String,
    pub line: u32,
    pub character: u32,
    /// Optional filter string to match against action title.
    pub filter: Option<String>,
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
    client_manager: Arc<ClientManager>,
    doc_manager: Arc<Mutex<DocumentManager>>,
    runtime: Handle,
    config: Config,
    broadcaster: EventBroadcaster,
}

impl LspBridgeHandler {
    /// Creates a new `LspBridgeHandler`.
    pub const fn new(
        client_manager: Arc<ClientManager>,
        doc_manager: Arc<Mutex<DocumentManager>>,
        runtime: Handle,
        config: Config,
        broadcaster: EventBroadcaster,
    ) -> Self {
        Self {
            client_manager,
            doc_manager,
            runtime,
            config,
            broadcaster,
        }
    }
    /// Gets the appropriate LSP client for the given file path.
    async fn get_client_for_path(&self, path: &Path) -> Result<Arc<Mutex<LspClient>>> {
        let lang_id = {
            let doc_manager = self.doc_manager.lock().await;
            doc_manager.language_id_for_path(path).to_string()
        };

        self.client_manager.get_client(&lang_id).await
    }

    /// Waits for the server handling the given path to be ready.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "Client lock held across wait_ready call"
    )]
    async fn wait_for_server_ready(&self, path: &Path) -> Result<()> {
        let (lang, is_ready) = {
            let client_mutex = self.get_client_for_path(path).await?;
            let client = client_mutex.lock().await;
            let lang = client.language().to_string();
            let ready = client.wait_ready().await;
            (lang, ready)
        };

        if !is_ready {
            return Err(anyhow!(
                "[{lang}] server died while waiting for ready state"
            ));
        }

        Ok(())
    }

    /// Extract file path from arguments if present.
    fn extract_file_path(arguments: Option<&serde_json::Value>) -> Option<PathBuf> {
        arguments
            .and_then(|v| v.get("file"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
    }

    /// Handles the `status` tool.
    fn handle_status(&self) -> CallToolResult {
        let statuses = self
            .runtime
            .block_on(async { self.client_manager.all_server_status().await });

        if statuses.is_empty() {
            return CallToolResult::text("No LSP servers running");
        }

        let mut output = Vec::new();
        for status in statuses {
            let state_str = match status.state {
                ServerState::Initializing => "Initializing",
                ServerState::Indexing => "Indexing",
                ServerState::Ready => "Ready",
                ServerState::Dead => "Dead",
            };

            let mut line = format!(
                "{}: {} (uptime: {}s)",
                status.language, state_str, status.uptime_secs
            );

            if let Some(title) = &status.progress_title {
                use std::fmt::Write;
                let _ = write!(line, " - {title}");
                if let Some(pct) = status.progress_percentage {
                    let _ = write!(line, " {pct}%");
                }
                if let Some(msg) = &status.progress_message {
                    let _ = write!(line, " ({msg})");
                }
            }

            output.push(line);
        }

        CallToolResult::text(output.join("\n"))
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

        if let Some(notification) = doc_manager.ensure_open(path).await? {
            match notification {
                DocumentNotification::Open(params) => {
                    client.did_open(params).await?;
                }
                DocumentNotification::Change(params) => {
                    client.did_change(params).await?;
                }
            }
        }

        let uri = doc_manager.uri_for_path(path)?;
        drop(doc_manager);
        drop(client);
        Ok((uri, client_mutex.clone()))
    }

    fn parse_position_input(arguments: Option<serde_json::Value>) -> Result<PositionInput> {
        serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
            .map_err(|e| anyhow!("Invalid arguments: {e}"))
    }

    /// Resolves a file path, converting relative paths to absolute using the current working directory.
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

    /// Resolves a [`SymbolOrPositionInput`] to a `(PathBuf, Position)`.
    ///
    /// If a symbol name is provided, delegates to [`resolve_symbol_position`].
    /// Otherwise, requires file/line/character and resolves the path.
    fn resolve_symbol_or_position(
        &self,
        input: &SymbolOrPositionInput,
    ) -> Result<(PathBuf, Position)> {
        if let Some(symbol) = &input.symbol {
            self.resolve_symbol_position(symbol, input.file.as_deref())
        } else {
            let file = input.file.as_ref().ok_or_else(|| {
                anyhow!("Either 'symbol' or 'file' with 'line'/'character' is required")
            })?;
            let line = input
                .line
                .ok_or_else(|| anyhow!("'line' is required when using position"))?;
            let character = input
                .character
                .ok_or_else(|| anyhow!("'character' is required when using position"))?;
            let path = Self::resolve_path(file)?;
            Ok((path, Position { line, character }))
        }
    }

    fn handle_hover(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        let input: SymbolOrPositionInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;
        let (path, position) = self.resolve_symbol_or_position(&input)?;

        debug!("Hover request: {}:{}", path.display(), position.line);

        let result = self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(&path).await?;
            let params = HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            };
            client_mutex.lock().await.hover(params).await
        })?;

        result.map_or_else(
            || Ok(CallToolResult::text("No hover information available")),
            |hover| Ok(CallToolResult::text(format_hover(&hover))),
        )
    }

    fn handle_definition(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        let input: SymbolOrPositionInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;
        let (path, position) = self.resolve_symbol_or_position(&input)?;

        debug!("Definition request: {}:{}", path.display(), position.line);

        let result = self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(&path).await?;
            let params = GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            };
            client_mutex.lock().await.definition(params).await
        })?;

        result.map_or_else(
            || Ok(CallToolResult::text("No definition found")),
            |response| Ok(CallToolResult::text(format_definition_response(&response))),
        )
    }

    fn handle_type_definition(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input: SymbolOrPositionInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;
        let (path, position) = self.resolve_symbol_or_position(&input)?;

        debug!(
            "Type definition request: {}:{}",
            path.display(),
            position.line
        );

        let result = self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(&path).await?;
            let params = GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            };
            client_mutex.lock().await.type_definition(params).await
        })?;

        result.map_or_else(
            || Ok(CallToolResult::text("No type definition found")),
            |response| Ok(CallToolResult::text(format_definition_response(&response))),
        )
    }

    fn handle_implementation(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input: SymbolOrPositionInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;
        let (path, position) = self.resolve_symbol_or_position(&input)?;

        debug!(
            "Implementation request: {}:{}",
            path.display(),
            position.line
        );

        let result = self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(&path).await?;
            let params = GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            };
            client_mutex.lock().await.implementation(params).await
        })?;

        result.map_or_else(
            || Ok(CallToolResult::text("No implementations found")),
            |response| Ok(CallToolResult::text(format_definition_response(&response))),
        )
    }

    fn handle_find_references(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input: FindReferencesInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        // Resolve target position - either from symbol search or direct position
        let sym_input = SymbolOrPositionInput {
            symbol: input.symbol,
            file: input.file,
            line: input.line,
            character: input.character,
        };
        let (target_path, target_position) = self.resolve_symbol_or_position(&sym_input)?;

        let (references, definition) = self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(&target_path).await?;

            let ref_params = ReferenceParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position: target_position,
                },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
                context: ReferenceContext {
                    include_declaration: input.include_declaration,
                },
            };

            let def_params = GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: target_position,
                },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            };

            let client = client_mutex.lock().await;
            let refs = client.references(ref_params).await?;
            let def = client.definition(def_params).await?;
            drop(client);
            Ok::<_, anyhow::Error>((refs, def))
        })?;

        match references {
            Some(locations) if !locations.is_empty() => {
                let def_loc = definition.as_ref().and_then(extract_definition_location);
                Ok(CallToolResult::text(format_locations_with_definition(
                    &locations,
                    def_loc.as_ref(),
                )))
            }
            _ => Ok(CallToolResult::text("No references found")),
        }
    }

    /// Resolve a symbol name to a file path and position.
    /// If `scope_file` is provided, searches within that file first.
    fn resolve_symbol_position(
        &self,
        symbol: &str,
        scope_file: Option<&str>,
    ) -> Result<(std::path::PathBuf, Position)> {
        // If a file is provided, try document symbols first for efficiency
        if let Some(file) = scope_file {
            let path = Self::resolve_path(file)?;
            if let Some(result) = self.find_symbol_in_document(symbol, &path)? {
                return Ok(result);
            }
        }

        // Fall back to workspace symbol search
        self.find_symbol_in_workspace(symbol)
    }

    fn find_symbol_in_document(
        &self,
        symbol: &str,
        path: &std::path::Path,
    ) -> Result<Option<(std::path::PathBuf, Position)>> {
        let result = self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(path).await?;
            let params = DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            };
            let response = client_mutex.lock().await.document_symbols(params).await?;
            Ok::<_, anyhow::Error>((uri, response))
        })?;

        let (uri, response) = result;
        if let Some(response) = response
            && let Some(position) = find_symbol_in_document_response(&response, symbol)
        {
            let file_path = std::path::PathBuf::from(uri.path().as_str());
            return Ok(Some((file_path, position)));
        }
        Ok(None)
    }

    /// Search for a symbol across the entire workspace.
    fn find_symbol_in_workspace(&self, symbol: &str) -> Result<(std::path::PathBuf, Position)> {
        let result = self.runtime.block_on(async {
            let params = WorkspaceSymbolParams {
                query: symbol.to_string(),
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            };

            let clients = self.client_manager.active_clients().await;

            for client_mutex in clients.values() {
                if let Ok(Some(response)) = client_mutex
                    .lock()
                    .await
                    .workspace_symbols(params.clone())
                    .await
                    && let Some((path, position)) =
                        find_symbol_in_workspace_response(&response, symbol)
                {
                    return Ok((path, position));
                }
            }

            Err(anyhow!("Symbol '{symbol}' not found in workspace"))
        })?;

        Ok(result)
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

            if input.wait_for_reanalysis && !client_mutex.lock().await.wait_for_analysis().await {
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

    /// Unified search: LSP workspace symbols with grep fallback.
    fn handle_search(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        let input: SearchInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        debug!("Search request: query={}", input.query);

        // First try workspace symbols (fast path)
        let (workspace_result, warnings) = self.runtime.block_on(async {
            let params = WorkspaceSymbolParams {
                query: input.query.clone(),
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            };

            let clients = self.client_manager.active_clients().await;
            let mut results = Vec::new();
            let mut warnings = Vec::new();

            for (lang, client_mutex) in &clients {
                match client_mutex
                    .lock()
                    .await
                    .workspace_symbols(params.clone())
                    .await
                {
                    Ok(Some(response)) => results.push(response),
                    Ok(None) => {}
                    Err(e) => {
                        warn!("[{lang}] workspace symbol search failed: {e}");
                        warnings.push(format!(
                            "Warning: [{lang}] unavailable, results may be incomplete"
                        ));
                    }
                }
            }

            (results, warnings)
        });

        // Check if we got any symbols from workspace search
        let has_results = workspace_result
            .iter()
            .any(|r| !format_workspace_symbols(r).contains("No symbols found"));

        if has_results {
            let mut text = if warnings.is_empty() {
                String::new()
            } else {
                format!("{}\n\n", warnings.join("\n"))
            };
            text.push_str(
                &workspace_result
                    .iter()
                    .map(format_workspace_symbols)
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            return Ok(CallToolResult::text(text));
        }

        // Fallback: search files with ripgrep, then query document symbols
        debug!("Workspace symbols found nothing, trying fallback search");
        Ok(self.search_fallback(&input.query, &warnings))
    }

    /// Fallback search: ripgrep for files, then document symbols.
    /// Adds a note about grep limitations when LSP workspace symbols were unavailable.
    fn search_fallback(&self, query: &str, warnings: &[String]) -> CallToolResult {
        const MAX_FILES: usize = 20;

        let mut output = String::new();
        if !warnings.is_empty() {
            output.push_str(&warnings.join("\n"));
            output.push('\n');
        }

        let roots = self.runtime.block_on(self.client_manager.roots());

        // Try ripgrep to find files, then query document symbols
        let files = Self::ripgrep_search(query, &roots)
            .unwrap_or_else(|_| Self::manual_file_search(query, &roots));

        if files.is_empty() {
            output.push_str("No results found");
            return CallToolResult::text(output);
        }

        let files: Vec<_> = files.into_iter().take(MAX_FILES).collect();
        debug!("Searching {} files for '{}'", files.len(), query);

        // Query document symbols for each file and filter
        let mut found_symbols = Vec::new();
        let query_lower = query.to_lowercase();

        for file_path in &files {
            if let Ok(Some(symbols)) = self.get_matching_symbols(file_path, &query_lower) {
                found_symbols.extend(symbols);
            }
        }

        if found_symbols.is_empty() {
            // No LSP symbols matched — fall back to raw grep line matches
            output.push_str(
                "Note: text search only (cannot distinguish definitions from usages).\n\n",
            );
            let grep_lines = Self::ripgrep_search_lines(query, &roots);
            if grep_lines.is_empty() {
                output.push_str("No results found");
            } else {
                output.push_str(&grep_lines);
            }
        } else {
            output.push_str(&found_symbols.join("\n"));
        }

        CallToolResult::text(output)
    }

    /// Use ripgrep to find files containing the query across workspace roots.
    fn ripgrep_search(query: &str, roots: &[PathBuf]) -> Result<Vec<std::path::PathBuf>> {
        use std::process::Command;

        let mut cmd = Command::new("rg");
        cmd.args([
            "--files-with-matches",
            "--ignore-case",
            "--type-add",
            "code:*.{rs,py,js,ts,tsx,jsx,go,java,c,cpp,h,hpp,cs,rb,php,swift,kt,scala,lua,sh,bash,zsh}",
            "--type",
            "code",
            query,
        ]);

        for root in roots {
            cmd.arg(root);
        }

        let output = cmd
            .output()
            .map_err(|e| anyhow!("Failed to run ripgrep: {e}"))?;

        if !output.status.success() && output.stdout.is_empty() {
            return Ok(Vec::new());
        }

        let files: Vec<std::path::PathBuf> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(std::path::PathBuf::from)
            .collect();

        Ok(files)
    }

    /// Use ripgrep to get line-level matches (`file:line:content`) across workspace roots.
    fn ripgrep_search_lines(query: &str, roots: &[PathBuf]) -> String {
        use std::process::Command;

        let mut cmd = Command::new("rg");
        cmd.args([
            "--line-number",
            "--no-heading",
            "--max-count",
            "5",
            "--ignore-case",
            "--type-add",
            "code:*.{rs,py,js,ts,tsx,jsx,go,java,c,cpp,h,hpp,cs,rb,php,swift,kt,scala,lua,sh,bash,zsh}",
            "--type",
            "code",
            query,
        ]);

        for root in roots {
            cmd.arg(root);
        }

        let Ok(output) = cmd.output() else {
            return String::new();
        };

        if !output.status.success() && output.stdout.is_empty() {
            return String::new();
        }

        let text = String::from_utf8_lossy(&output.stdout);
        // Cap output to 50 lines
        text.lines().take(50).collect::<Vec<_>>().join("\n")
    }

    /// Manual file search fallback when ripgrep is not available.
    fn manual_file_search(query: &str, roots: &[PathBuf]) -> Vec<std::path::PathBuf> {
        let query_lower = query.to_lowercase();
        let mut matches = Vec::new();

        let mut builder = WalkBuilder::new(roots.first().map_or_else(
            || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            Clone::clone,
        ));
        for root in roots.iter().skip(1) {
            builder.add(root);
        }
        let walker = builder
            .hidden(true)
            .git_ignore(true)
            .max_depth(Some(10))
            .build();

        for entry in walker.flatten() {
            if matches.len() >= 50 {
                break; // Cap search
            }

            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            // Only search code files
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !matches!(
                ext,
                "rs" | "py"
                    | "js"
                    | "ts"
                    | "tsx"
                    | "jsx"
                    | "go"
                    | "java"
                    | "c"
                    | "cpp"
                    | "h"
                    | "hpp"
                    | "cs"
                    | "rb"
                    | "php"
                    | "swift"
                    | "kt"
                    | "scala"
            ) {
                continue;
            }

            // Read and search file content
            if let Ok(content) = std::fs::read_to_string(path)
                && content.to_lowercase().contains(&query_lower)
            {
                matches.push(path.to_path_buf());
            }
        }

        matches
    }

    /// Get document symbols from a file that match the query.
    fn get_matching_symbols(
        &self,
        path: &std::path::Path,
        query_lower: &str,
    ) -> Result<Option<Vec<String>>> {
        let result = self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(path).await?;
            let params = DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            };
            client_mutex.lock().await.document_symbols(params).await
        })?;

        let Some(response) = result else {
            return Ok(None);
        };

        let symbols = filter_matching_symbols(&response, query_lower, path);
        if symbols.is_empty() {
            Ok(None)
        } else {
            Ok(Some(symbols))
        }
    }

    fn handle_code_actions(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        let input: CodeActionInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let path = Self::resolve_path(&input.file)?;

        debug!(
            "Code actions request: {} [{},{}]-[{},{}]",
            input.file,
            input.start_line,
            input.start_character,
            input.end_line,
            input.end_character
        );

        let result = self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(&path).await?;

            // Get diagnostics for the range to include in context
            let diagnostics = client_mutex.lock().await.get_diagnostics(&uri).await;

            let params = CodeActionParams {
                text_document: TextDocumentIdentifier { uri },
                range: Range {
                    start: Position {
                        line: input.start_line,
                        character: input.start_character,
                    },
                    end: Position {
                        line: input.end_line,
                        character: input.end_character,
                    },
                },
                context: CodeActionContext {
                    diagnostics,
                    only: None,
                    trigger_kind: None,
                },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            };
            client_mutex.lock().await.code_actions(params).await
        })?;

        match result {
            Some(actions) if !actions.is_empty() => {
                Ok(CallToolResult::text(format_code_actions(&actions)))
            }
            _ => Ok(CallToolResult::text("No code actions available")),
        }
    }

    fn handle_rename(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        let input: RenameInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let path = Self::resolve_path(&input.file)?;

        debug!(
            "Rename request: {}:{}:{} -> {}",
            input.file, input.line, input.character, input.new_name
        );

        let result = self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(&path).await?;
            let params = RenameParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position {
                        line: input.line,
                        character: input.character,
                    },
                },
                new_name: input.new_name,
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            };
            client_mutex.lock().await.rename(params).await
        })?;

        Ok(result.map_or_else(
            || CallToolResult::text("Rename not supported at this location"),
            |edit| CallToolResult::text(format_workspace_edit(&edit)),
        ))
    }

    fn handle_completion(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        let input = Self::parse_position_input(arguments)?;
        let path = Self::resolve_path(&input.file)?;

        debug!(
            "Completion request: {}:{}:{}",
            input.file, input.line, input.character
        );

        let result = self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(&path).await?;
            let params = CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position {
                        line: input.line,
                        character: input.character,
                    },
                },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
                context: None,
            };
            client_mutex.lock().await.completion(params).await
        })?;

        result.map_or_else(
            || Ok(CallToolResult::text("No completions available")),
            |response| Ok(CallToolResult::text(format_completion(&response))),
        )
    }

    fn handle_diagnostics(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        let input: FileInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let path = Self::resolve_path(&input.file)?;

        debug!("Diagnostics request: {}", input.file);

        let diagnostics = self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(&path).await?;

            if input.wait_for_reanalysis && !client_mutex.lock().await.wait_for_analysis().await {
                return Err(anyhow!("LSP server stopped responding during analysis"));
            }

            Ok::<_, anyhow::Error>(client_mutex.lock().await.get_diagnostics(&uri).await)
        })?;

        if diagnostics.is_empty() {
            Ok(CallToolResult::text("No diagnostics"))
        } else {
            Ok(CallToolResult::text(format_diagnostics(&diagnostics)))
        }
    }

    fn handle_signature_help(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input = Self::parse_position_input(arguments)?;
        let path = Self::resolve_path(&input.file)?;

        debug!(
            "Signature help request: {}:{}:{}",
            input.file, input.line, input.character
        );

        let result = self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(&path).await?;
            let params = SignatureHelpParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position {
                        line: input.line,
                        character: input.character,
                    },
                },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                context: None,
            };
            client_mutex.lock().await.signature_help(params).await
        })?;

        result.map_or_else(
            || Ok(CallToolResult::text("No signature help available")),
            |help| Ok(CallToolResult::text(format_signature_help(&help))),
        )
    }

    fn handle_formatting(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        let input: FormattingInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let path = Self::resolve_path(&input.file)?;

        debug!("Formatting request: {}", input.file);

        let result = self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(&path).await?;
            let params = DocumentFormattingParams {
                text_document: TextDocumentIdentifier { uri },
                options: FormattingOptions {
                    tab_size: input.tab_size,
                    insert_spaces: input.insert_spaces,
                    ..lsp_types::FormattingOptions::default()
                },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            };
            client_mutex.lock().await.formatting(params).await
        })?;

        match result {
            Some(edits) if !edits.is_empty() => Ok(CallToolResult::text(format_text_edits(&edits))),
            _ => Ok(CallToolResult::text("No formatting changes")),
        }
    }

    fn handle_range_formatting(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input: RangeFormattingInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let path = Self::resolve_path(&input.file)?;

        debug!(
            "Range formatting request: {} [{},{}]-[{},{}]",
            input.file,
            input.start_line,
            input.start_character,
            input.end_line,
            input.end_character
        );

        let result = self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(&path).await?;
            let params = DocumentRangeFormattingParams {
                text_document: TextDocumentIdentifier { uri },
                range: Range {
                    start: Position {
                        line: input.start_line,
                        character: input.start_character,
                    },
                    end: Position {
                        line: input.end_line,
                        character: input.end_character,
                    },
                },
                options: FormattingOptions {
                    tab_size: input.tab_size,
                    insert_spaces: input.insert_spaces,
                    ..lsp_types::FormattingOptions::default()
                },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            };
            client_mutex.lock().await.range_formatting(params).await
        })?;

        match result {
            Some(edits) if !edits.is_empty() => Ok(CallToolResult::text(format_text_edits(&edits))),
            _ => Ok(CallToolResult::text("No formatting changes")),
        }
    }

    fn handle_call_hierarchy(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input: CallHierarchyInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let sym_input = SymbolOrPositionInput {
            symbol: input.symbol,
            file: input.file,
            line: input.line,
            character: input.character,
        };
        let (path, position) = self.resolve_symbol_or_position(&sym_input)?;

        debug!(
            "Call hierarchy request: {}:{} direction={}",
            path.display(),
            position.line,
            input.direction
        );

        let result = self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(&path).await?;

            // First, prepare the call hierarchy
            let prepare_params = CallHierarchyPrepareParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            };

            let client = client_mutex.lock().await;
            let items = client.prepare_call_hierarchy(prepare_params).await?;

            let Some(items) = items else {
                return Ok::<_, anyhow::Error>(None);
            };

            if items.is_empty() {
                return Ok(None);
            }

            // Get calls for the first item (safe: we checked is_empty above)
            let Some(item) = items.into_iter().next() else {
                return Ok(None);
            };

            match input.direction.as_str() {
                "incoming" => {
                    let params = CallHierarchyIncomingCallsParams {
                        item,
                        work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                        partial_result_params: lsp_types::PartialResultParams::default(),
                    };
                    let calls = client.incoming_calls(params).await?;
                    drop(client);
                    Ok(calls.map(|c| format_incoming_calls(&c)))
                }
                "outgoing" => {
                    let params = CallHierarchyOutgoingCallsParams {
                        item,
                        work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                        partial_result_params: lsp_types::PartialResultParams::default(),
                    };
                    let calls = client.outgoing_calls(params).await?;
                    drop(client);
                    Ok(calls.map(|c| format_outgoing_calls(&c)))
                }
                _ => Err(anyhow!("direction must be 'incoming' or 'outgoing'")),
            }
        })?;

        match result {
            Some(text) if !text.is_empty() => Ok(CallToolResult::text(text)),
            _ => Ok(CallToolResult::text("No call hierarchy found")),
        }
    }

    fn handle_type_hierarchy(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input: TypeHierarchyInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let sym_input = SymbolOrPositionInput {
            symbol: input.symbol,
            file: input.file,
            line: input.line,
            character: input.character,
        };
        let (path, position) = self.resolve_symbol_or_position(&sym_input)?;

        debug!(
            "Type hierarchy request: {}:{} direction={}",
            path.display(),
            position.line,
            input.direction
        );

        let result = self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(&path).await?;

            // First, prepare the type hierarchy
            let prepare_params = TypeHierarchyPrepareParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            };

            let client = client_mutex.lock().await;
            let items = client.prepare_type_hierarchy(prepare_params).await?;

            let Some(items) = items else {
                return Ok::<_, anyhow::Error>(None);
            };

            if items.is_empty() {
                return Ok(None);
            }

            // Get hierarchy for the first item (safe: we checked is_empty above)
            let Some(item) = items.into_iter().next() else {
                return Ok(None);
            };

            match input.direction.as_str() {
                "supertypes" => {
                    let params = TypeHierarchySupertypesParams {
                        item,
                        work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                        partial_result_params: lsp_types::PartialResultParams::default(),
                    };
                    let types = client.supertypes(params).await?;
                    drop(client);
                    Ok(types.map(|t| format_type_hierarchy_items(&t)))
                }
                "subtypes" => {
                    let params = TypeHierarchySubtypesParams {
                        item,
                        work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                        partial_result_params: lsp_types::PartialResultParams::default(),
                    };
                    let types = client.subtypes(params).await?;
                    drop(client);
                    Ok(types.map(|t| format_type_hierarchy_items(&t)))
                }
                _ => Err(anyhow!("direction must be 'supertypes' or 'subtypes'")),
            }
        })?;

        match result {
            Some(text) if !text.is_empty() => Ok(CallToolResult::text(text)),
            _ => Ok(CallToolResult::text("No type hierarchy found")),
        }
    }
    #[allow(
        clippy::too_many_lines,
        clippy::significant_drop_tightening,
        reason = "Complexity of quickfix selection requires many lines; client lock held across async operations"
    )]
    fn handle_apply_quickfix(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input: ApplyQuickFixInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let path = Self::resolve_path(&input.file)?;

        debug!(
            "Apply quickfix request: {}:{}:{} filter={:?}",
            input.file, input.line, input.character, input.filter
        );

        let result = self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(&path).await?;
            let client = client_mutex.lock().await;

            // 1. Get diagnostics to find the relevant range and context
            let diagnostics = client.get_diagnostics(&uri).await;

            // Find diagnostic at cursor
            let cursor_line = input.line;
            let cursor_char = input.character;

            let target_diagnostic = diagnostics.iter().find(|d| {
                let start = d.range.start;
                let end = d.range.end;

                // Check if cursor is within range (inclusive of start, exclusive of end usually, but let's be loose)
                if cursor_line < start.line || cursor_line > end.line {
                    return false;
                }
                if cursor_line == start.line && cursor_char < start.character {
                    return false;
                }
                if cursor_line == end.line && cursor_char > end.character {
                    return false;
                }
                true
            });

            let (range, context_diagnostics) = target_diagnostic.map_or_else(
                || {
                    (
                        Range {
                            start: Position {
                                line: cursor_line,
                                character: cursor_char,
                            },
                            end: Position {
                                line: cursor_line,
                                character: cursor_char,
                            },
                        },
                        vec![],
                    )
                },
                |d| (d.range, vec![d.clone()]),
            );

            // 2. Request Code Actions
            let params = CodeActionParams {
                text_document: TextDocumentIdentifier { uri },
                range,
                context: CodeActionContext {
                    diagnostics: context_diagnostics,
                    only: None,
                    trigger_kind: None,
                },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            };

            let response = client.code_actions(params).await?;
            let actions = response.unwrap_or_default();

            if actions.is_empty() {
                return Err(anyhow!("No code actions available at this location"));
            }

            // 3. Filter and Pick Action
            let action_to_apply = if let Some(filter) = &input.filter {
                actions.into_iter().find(|a| match a {
                    CodeActionOrCommand::Command(cmd) => cmd.title.contains(filter),
                    CodeActionOrCommand::CodeAction(ca) => ca.title.contains(filter),
                })
            } else {
                // Prefer "quickfix" kind
                let quickfix = actions.iter().find(|a| match a {
                    CodeActionOrCommand::CodeAction(ca) => {
                        ca.kind
                            .as_ref()
                            .is_some_and(|k| k.as_str().contains("quickfix"))
                    }
                    CodeActionOrCommand::Command(_) => false,
                });

                quickfix.cloned().or_else(|| actions.first().cloned())
            };

            let Some(action) = action_to_apply else {
                return Err(anyhow!("No matching code action found"));
            };

            // 4. Return proposed edits
            match action {
                CodeActionOrCommand::Command(cmd) => {
                    Err(anyhow!("Selected action is a Command ('{}'), not a WorkspaceEdit. Cannot extract proposed edits.", cmd.title))
                }
                CodeActionOrCommand::CodeAction(mut ca) => {
                    // Resolve if edit is missing (lazy resolution)
                    if ca.edit.is_none() {
                        debug!("Resolving code action: {}", ca.title);
                        ca = client.resolve_code_action(ca).await?;
                    }

                    if let Some(edit) = ca.edit {
                        Ok(format!(
                            "Proposed fix: {}\n{}",
                            ca.title,
                            format_workspace_edit(&edit)
                        ))
                    } else {
                        Err(anyhow!("Code action '{}' resolved but still has no edit attached", ca.title))
                    }
                }
            }
        });

        match result {
            Ok(msg) => Ok(CallToolResult::text(msg)),
            Err(e) => Ok(CallToolResult::error(e.to_string())),
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

                    if lang_id == "plaintext" {
                        continue;
                    }

                    if let Ok(client_mutex) = self.client_manager.get_client(&lang_id).await {
                        // Attempt to open and get symbols with a short timeout
                        if let Ok((uri, _)) = self.ensure_document_open(&entry.path).await {
                            let params = DocumentSymbolParams {
                                text_document: TextDocumentIdentifier { uri },
                                work_done_progress_params:
                                    lsp_types::WorkDoneProgressParams::default(),
                                partial_result_params: lsp_types::PartialResultParams::default(),
                            };

                            let client = client_mutex.lock().await;
                            let symbols_future = client.document_symbols(params);

                            // 1s timeout per file to keep map generation snappy but reliable
                            let timeout_result = tokio::time::timeout(
                                std::time::Duration::from_secs(1),
                                symbols_future,
                            )
                            .await;
                            drop(client);

                            if let Ok(Ok(Some(response))) = timeout_result {
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
    #[allow(clippy::too_many_lines, reason = "Naturally long list of tools")]
    fn list_tools(&self) -> Vec<Tool> {
        vec![
            Tool {
                name: "hover".to_string(),
                description: Some("Get hover information (documentation, type info) for a symbol. Accepts a symbol name or file/line/character position.".to_string()),
                input_schema: symbol_or_position_schema(),
            },
            Tool {
                name: "definition".to_string(),
                description: Some("Go to the definition of a symbol. Accepts a symbol name or file/line/character position.".to_string()),
                input_schema: symbol_or_position_schema(),
            },
            Tool {
                name: "type_definition".to_string(),
                description: Some("Go to the type definition of a symbol (e.g., for a variable, go to its type's definition). Accepts a symbol name or file/line/character position.".to_string()),
                input_schema: symbol_or_position_schema(),
            },
            Tool {
                name: "implementation".to_string(),
                description: Some("Find implementations of an interface, trait, or abstract method. Accepts a symbol name or file/line/character position.".to_string()),
                input_schema: symbol_or_position_schema(),
            },
            Tool {
                name: "find_references".to_string(),
                description: Some("Find all references to a symbol. Accepts either a symbol name (searched across workspace) or a file/line/character position. The definition is marked with [def] in results.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "symbol": { "type": "string", "description": "Symbol name to search for (e.g., 'MyClass', 'handleRequest'). If provided, the symbol will be found via workspace search." },
                        "file": { "type": "string", "description": "Absolute or relative path to the file. Required if using line/character position; optional with symbol to narrow search scope." },
                        "line": { "type": "integer", "description": "Line number (0-indexed). Required if not using symbol." },
                        "character": { "type": "integer", "description": "Character position (0-indexed). Required if not using symbol." },
                        "include_declaration": { "type": "boolean", "description": "Include the declaration in results (default: true)" }
                    }
                }),
            },
            Tool {
                name: "document_symbols".to_string(),
                description: Some("Get the symbol outline of a file (functions, classes, variables, etc.).".to_string()),
                input_schema: file_schema(),
            },
            Tool {
                name: "search".to_string(),
                description: Some("Search for a symbol or pattern across the workspace. Uses LSP workspace symbols when available, falls back to text search. Warns when using fallback since text search cannot distinguish definitions from usages.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Symbol name or text pattern to search for" }
                    },
                    "required": ["query"]
                }),
            },
            Tool {
                name: "code_actions".to_string(),
                description: Some("Get available code actions (quick fixes, refactorings) for a range.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Absolute path to the file" },
                        "start_line": { "type": "integer", "description": "Start line (0-indexed)" },
                        "start_character": { "type": "integer", "description": "Start character (0-indexed)" },
                        "end_line": { "type": "integer", "description": "End line (0-indexed)" },
                        "end_character": { "type": "integer", "description": "End character (0-indexed)" }
                    },
                    "required": ["file", "start_line", "start_character", "end_line", "end_character"]
                }),
            },
            Tool {
                name: "rename".to_string(),
                description: Some("Compute the edits needed to rename a symbol across the codebase. Returns proposed changes — does not modify files.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Absolute path to the file" },
                        "line": { "type": "integer", "description": "Line number (0-indexed)" },
                        "character": { "type": "integer", "description": "Character position (0-indexed)" },
                        "new_name": { "type": "string", "description": "New name for the symbol" }
                    },
                    "required": ["file", "line", "character", "new_name"]
                }),
            },
            Tool {
                name: "completion".to_string(),
                description: Some("Get completion suggestions at a position.".to_string()),
                input_schema: position_schema(),
            },
            Tool {
                name: "diagnostics".to_string(),
                description: Some("Get diagnostics (errors, warnings, hints) for a file.".to_string()),
                input_schema: file_schema(),
            },
            Tool {
                name: "signature_help".to_string(),
                description: Some("Get function signature help at a position (parameter info while typing a call).".to_string()),
                input_schema: position_schema(),
            },
            Tool {
                name: "formatting".to_string(),
                description: Some("Format an entire document.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Absolute path to the file" },
                        "tab_size": { "type": "integer", "description": "Tab size (default: 4)" },
                        "insert_spaces": { "type": "boolean", "description": "Use spaces instead of tabs (default: false)" }
                    },
                    "required": ["file"]
                }),
            },
            Tool {
                name: "range_formatting".to_string(),
                description: Some("Format a specific range within a document.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Absolute path to the file" },
                        "start_line": { "type": "integer", "description": "Start line (0-indexed)" },
                        "start_character": { "type": "integer", "description": "Start character (0-indexed)" },
                        "end_line": { "type": "integer", "description": "End line (0-indexed)" },
                        "end_character": { "type": "integer", "description": "End character (0-indexed)" },
                        "tab_size": { "type": "integer", "description": "Tab size (default: 4)" },
                        "insert_spaces": { "type": "boolean", "description": "Use spaces instead of tabs (default: false)" }
                    },
                    "required": ["file", "start_line", "start_character", "end_line", "end_character"]
                }),
            },
            Tool {
                name: "call_hierarchy".to_string(),
                description: Some("Get incoming or outgoing calls for a function/method. Accepts a symbol name or file/line/character position.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "symbol": { "type": "string", "description": "Symbol name to search for (e.g., 'MyStruct', 'handle_request'). If provided, position fields are optional." },
                        "file": { "type": "string", "description": "Absolute or relative path to the file. Required when using line/character; optional with symbol to narrow search." },
                        "line": { "type": "integer", "description": "Line number (0-indexed). Required if not using symbol." },
                        "character": { "type": "integer", "description": "Character position (0-indexed). Required if not using symbol." },
                        "direction": { "type": "string", "enum": ["incoming", "outgoing"], "description": "Direction: 'incoming' (who calls this?) or 'outgoing' (what does this call?)" }
                    },
                    "required": ["direction"]
                }),
            },
            Tool {
                name: "type_hierarchy".to_string(),
                description: Some("Get supertypes or subtypes of a type. Accepts a symbol name or file/line/character position.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "symbol": { "type": "string", "description": "Symbol name to search for (e.g., 'MyStruct', 'handle_request'). If provided, position fields are optional." },
                        "file": { "type": "string", "description": "Absolute or relative path to the file. Required when using line/character; optional with symbol to narrow search." },
                        "line": { "type": "integer", "description": "Line number (0-indexed). Required if not using symbol." },
                        "character": { "type": "integer", "description": "Character position (0-indexed). Required if not using symbol." },
                        "direction": { "type": "string", "enum": ["supertypes", "subtypes"], "description": "Direction: 'supertypes' (parent types) or 'subtypes' (child types)" }
                    },
                    "required": ["direction"]
                }),
            },
            Tool {
                name: "status".to_string(),
                description: Some("Report the status of all LSP servers (state, progress, uptime).".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            Tool {
                name: "apply_quickfix".to_string(),
                description: Some("Find a Code Action (Quick Fix) for a diagnostic at the given position and return its proposed edits. Does not modify files.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Absolute path to the file" },
                        "line": { "type": "integer", "description": "Line number (0-indexed)" },
                        "character": { "type": "integer", "description": "Character position (0-indexed)" },
                        "filter": { "type": "string", "description": "Optional text to match against the action title (e.g. 'Import')" }
                    },
                    "required": ["file", "line", "character"]
                }),
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
        ]
    }

    fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let start = std::time::Instant::now();
        let file =
            Self::extract_file_path(arguments.as_ref()).map(|p| p.to_string_lossy().to_string());

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

        // Handle status tool separately (no file path)
        if name == "status" {
            let result = self.handle_status();
            broadcast_result(result.is_error.is_none());
            return Ok(result);
        }

        // Smart wait for methods that need a ready server
        if self.config.smart_wait
            && METHODS_WAIT_FOR_READY.contains(&name)
            && let Some(ref path) = Self::extract_file_path(arguments.as_ref())
            && let Err(e) = self.runtime.block_on(self.wait_for_server_ready(path))
        {
            broadcast_result(false);
            return Err(e);
        }

        let result = match name {
            "hover" => self.handle_hover(arguments),
            "definition" => self.handle_definition(arguments),
            "type_definition" => self.handle_type_definition(arguments),
            "implementation" => self.handle_implementation(arguments),
            "find_references" => self.handle_find_references(arguments),
            "document_symbols" => self.handle_document_symbols(arguments),
            "search" => self.handle_search(arguments),
            "code_actions" => self.handle_code_actions(arguments),
            "rename" => self.handle_rename(arguments),
            "completion" => self.handle_completion(arguments),
            "diagnostics" => self.handle_diagnostics(arguments),
            "signature_help" => self.handle_signature_help(arguments),
            "formatting" => self.handle_formatting(arguments),
            "range_formatting" => self.handle_range_formatting(arguments),
            "call_hierarchy" => self.handle_call_hierarchy(arguments),
            "type_hierarchy" => self.handle_type_hierarchy(arguments),
            "apply_quickfix" => self.handle_apply_quickfix(arguments),
            "codebase_map" => self.handle_codebase_map(arguments),
            _ => Err(anyhow!("Unknown tool: {name}")),
        };

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

// Schema helpers
fn position_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file": { "type": "string", "description": "Absolute path to the file" },
            "line": { "type": "integer", "description": "Line number (0-indexed)" },
            "character": { "type": "integer", "description": "Character position (0-indexed)" }
        },
        "required": ["file", "line", "character"]
    })
}

fn symbol_or_position_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "symbol": { "type": "string", "description": "Symbol name to search for (e.g., 'MyStruct', 'handle_request'). If provided, position fields are optional." },
            "file": { "type": "string", "description": "Absolute or relative path to the file. Required when using line/character; optional with symbol to narrow search." },
            "line": { "type": "integer", "description": "Line number (0-indexed). Required if not using symbol." },
            "character": { "type": "integer", "description": "Character position (0-indexed). Required if not using symbol." }
        }
    })
}

fn file_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file": { "type": "string", "description": "Absolute path to the file" },
            "wait_for_reanalysis": { "type": "boolean", "description": "Wait for LSP server to finish re-analyzing the file after changes (default: true)" }
        },
        "required": ["file"]
    })
}

// Formatting helpers
fn format_hover(hover: &Hover) -> String {
    use lsp_types::HoverContents;
    match &hover.contents {
        HoverContents::Scalar(marked_string) => format_marked_string(marked_string),
        HoverContents::Array(strings) => strings
            .iter()
            .map(format_marked_string)
            .collect::<Vec<_>>()
            .join("\n\n"),
        HoverContents::Markup(markup) => markup.value.clone(),
    }
}

fn format_marked_string(marked: &lsp_types::MarkedString) -> String {
    match marked {
        lsp_types::MarkedString::String(s) => s.clone(),
        lsp_types::MarkedString::LanguageString(ls) => {
            format!("```{}\n{}\n```", ls.language, ls.value)
        }
    }
}

fn format_definition_response(response: &GotoDefinitionResponse) -> String {
    match response {
        GotoDefinitionResponse::Scalar(location) => format_location(location),
        GotoDefinitionResponse::Array(locations) => {
            if locations.is_empty() {
                "No results".to_string()
            } else {
                locations
                    .iter()
                    .map(format_location)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        GotoDefinitionResponse::Link(links) => {
            if links.is_empty() {
                "No results".to_string()
            } else {
                links
                    .iter()
                    .map(format_location_link)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
    }
}

/// Find a symbol by name in a document symbol response, returning its position.
fn find_symbol_in_document_response(
    response: &DocumentSymbolResponse,
    name: &str,
) -> Option<Position> {
    match response {
        DocumentSymbolResponse::Flat(symbols) => {
            // Exact match first
            symbols
                .iter()
                .find(|s| s.name == name)
                .or_else(|| symbols.iter().find(|s| s.name.contains(name)))
                .map(|s| s.location.range.start)
        }
        DocumentSymbolResponse::Nested(symbols) => find_in_nested_symbols(symbols, name),
    }
}

/// Recursively search nested document symbols.
fn find_in_nested_symbols(symbols: &[DocumentSymbol], name: &str) -> Option<Position> {
    for symbol in symbols {
        if symbol.name == name {
            return Some(symbol.selection_range.start);
        }
        if let Some(children) = &symbol.children
            && let Some(pos) = find_in_nested_symbols(children, name)
        {
            return Some(pos);
        }
    }
    // Second pass: partial match
    for symbol in symbols {
        if symbol.name.contains(name) {
            return Some(symbol.selection_range.start);
        }
        if let Some(children) = &symbol.children
            && let Some(pos) = find_in_nested_symbols(children, name)
        {
            return Some(pos);
        }
    }
    None
}

/// Find a symbol by name in workspace symbol response, returning path and position.
fn find_symbol_in_workspace_response(
    response: &WorkspaceSymbolResponse,
    name: &str,
) -> Option<(std::path::PathBuf, Position)> {
    match response {
        WorkspaceSymbolResponse::Flat(symbols) => {
            // Exact match first
            let symbol = symbols
                .iter()
                .find(|s| s.name == name)
                .or_else(|| symbols.iter().find(|s| s.name.contains(name)))?;
            let path = std::path::PathBuf::from(symbol.location.uri.path().as_str());
            Some((path, symbol.location.range.start))
        }
        WorkspaceSymbolResponse::Nested(symbols) => {
            let symbol = symbols
                .iter()
                .find(|s| s.name == name)
                .or_else(|| symbols.iter().find(|s| s.name.contains(name)))?;
            match &symbol.location {
                lsp_types::OneOf::Left(location) => {
                    let path = std::path::PathBuf::from(location.uri.path().as_str());
                    Some((path, location.range.start))
                }
                lsp_types::OneOf::Right(_) => None, // URI-only location, can't get position
            }
        }
    }
}

fn format_location(location: &Location) -> String {
    let path = location.uri.path();
    let line = location.range.start.line + 1;
    let col = location.range.start.character + 1;
    format!("{path}:{line}:{col}")
}

fn format_location_link(loc_link: &LocationLink) -> String {
    let path = loc_link.target_uri.path();
    let line = loc_link.target_range.start.line + 1;
    let col = loc_link.target_range.start.character + 1;
    format!("{path}:{line}:{col}")
}

/// Format locations with the definition marked and listed first.
fn format_locations_with_definition(
    locations: &[Location],
    definition: Option<&Location>,
) -> String {
    // Check if a location matches the definition
    let is_definition = |loc: &Location| -> bool {
        definition.is_some_and(|def| loc.uri == def.uri && loc.range.start == def.range.start)
    };

    // Sort: definition first, then by file path and line
    let mut sorted: Vec<_> = locations.iter().collect();
    sorted.sort_by(|a, b| {
        let a_is_def = is_definition(a);
        let b_is_def = is_definition(b);
        match (a_is_def, b_is_def) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => {
                // Sort by path, then line, then column
                let path_cmp = a.uri.path().as_str().cmp(b.uri.path().as_str());
                if path_cmp != std::cmp::Ordering::Equal {
                    return path_cmp;
                }
                let line_cmp = a.range.start.line.cmp(&b.range.start.line);
                if line_cmp != std::cmp::Ordering::Equal {
                    return line_cmp;
                }
                a.range.start.character.cmp(&b.range.start.character)
            }
        }
    });

    sorted
        .iter()
        .map(|loc| {
            if is_definition(loc) {
                format!("{} [def]", format_location(loc))
            } else {
                format_location(loc)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract the first location from a `GotoDefinitionResponse`.
fn extract_definition_location(response: &GotoDefinitionResponse) -> Option<Location> {
    match response {
        GotoDefinitionResponse::Scalar(loc) => Some(loc.clone()),
        GotoDefinitionResponse::Array(locs) => locs.first().cloned(),
        GotoDefinitionResponse::Link(links) => links.first().map(|link| Location {
            uri: link.target_uri.clone(),
            range: link.target_selection_range,
        }),
    }
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

fn format_workspace_symbols(response: &WorkspaceSymbolResponse) -> String {
    match response {
        WorkspaceSymbolResponse::Flat(symbols) => {
            if symbols.is_empty() {
                "No symbols found".to_string()
            } else {
                symbols
                    .iter()
                    .map(format_symbol_info)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        WorkspaceSymbolResponse::Nested(symbols) => {
            if symbols.is_empty() {
                "No symbols found".to_string()
            } else {
                symbols
                    .iter()
                    .map(|s| {
                        let kind = format!("{:?}", s.kind);
                        let loc = match &s.location {
                            lsp_types::OneOf::Left(loc) => format_location(loc),
                            lsp_types::OneOf::Right(uri_info) => uri_info.uri.path().to_string(),
                        };
                        format!("{} [{}] {}", s.name, kind, loc)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
    }
}

/// Filter document symbols that match the query (case-insensitive).
fn filter_matching_symbols(
    response: &DocumentSymbolResponse,
    query_lower: &str,
    file_path: &std::path::Path,
) -> Vec<String> {
    let mut results = Vec::new();
    let file_str = file_path.display().to_string();

    match response {
        DocumentSymbolResponse::Flat(symbols) => {
            for symbol in symbols {
                if symbol.name.to_lowercase().contains(query_lower) {
                    let line = symbol.location.range.start.line + 1;
                    results.push(format!(
                        "{} [{:?}] {}:{}",
                        symbol.name, symbol.kind, file_str, line
                    ));
                }
            }
        }
        DocumentSymbolResponse::Nested(symbols) => {
            collect_matching_nested_symbols(symbols, query_lower, &file_str, &mut results);
        }
    }

    results
}

/// Recursively collect matching symbols from nested document symbols.
fn collect_matching_nested_symbols(
    symbols: &[DocumentSymbol],
    query_lower: &str,
    file_str: &str,
    results: &mut Vec<String>,
) {
    for symbol in symbols {
        if symbol.name.to_lowercase().contains(query_lower) {
            let line = symbol.selection_range.start.line + 1;
            results.push(format!(
                "{} [{:?}] {}:{}",
                symbol.name, symbol.kind, file_str, line
            ));
        }
        if let Some(children) = &symbol.children {
            collect_matching_nested_symbols(children, query_lower, file_str, results);
        }
    }
}

fn format_code_actions(actions: &[CodeActionOrCommand]) -> String {
    actions
        .iter()
        .enumerate()
        .map(|(i, action)| match action {
            CodeActionOrCommand::Command(cmd) => format!("{}. [Command] {}", i + 1, cmd.title),
            CodeActionOrCommand::CodeAction(ca) => {
                let kind = ca
                    .kind
                    .as_ref()
                    .map(|k| format!(" ({})", k.as_str()))
                    .unwrap_or_default();
                format!("{}. {}{}", i + 1, ca.title, kind)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_workspace_edit(edit: &WorkspaceEdit) -> String {
    let mut result = Vec::new();

    if let Some(changes) = &edit.changes {
        for (uri, edits) in changes {
            result.push(format!("File: {}", uri.path()));
            for e in edits {
                result.push(format!(
                    "  L{}:{}-L{}:{}: {}",
                    e.range.start.line + 1,
                    e.range.start.character + 1,
                    e.range.end.line + 1,
                    e.range.end.character + 1,
                    e.new_text.replace('\n', "\\n")
                ));
            }
        }
    }

    if let Some(doc_changes) = &edit.document_changes {
        match doc_changes {
            DocumentChanges::Edits(edits) => {
                for edit in edits {
                    result.push(format!("File: {}", edit.text_document.uri.path()));
                    for e in &edit.edits {
                        match e {
                            lsp_types::OneOf::Left(text_edit) => {
                                result.push(format!(
                                    "  L{}:{}-L{}:{}: {}",
                                    text_edit.range.start.line + 1,
                                    text_edit.range.start.character + 1,
                                    text_edit.range.end.line + 1,
                                    text_edit.range.end.character + 1,
                                    text_edit.new_text.replace('\n', "\\n")
                                ));
                            }
                            lsp_types::OneOf::Right(annotated) => {
                                result.push(format!(
                                    "  L{}:{}-L{}:{}: {}",
                                    annotated.text_edit.range.start.line + 1,
                                    annotated.text_edit.range.start.character + 1,
                                    annotated.text_edit.range.end.line + 1,
                                    annotated.text_edit.range.end.character + 1,
                                    annotated.text_edit.new_text.replace('\n', "\\n")
                                ));
                            }
                        }
                    }
                }
            }
            DocumentChanges::Operations(ops) => {
                for op in ops {
                    match op {
                        lsp_types::DocumentChangeOperation::Op(resource_op) => {
                            result.push(format!("Operation: {resource_op:?}"));
                        }
                        lsp_types::DocumentChangeOperation::Edit(edit) => {
                            result.push(format!("File: {}", edit.text_document.uri.path()));
                            for e in &edit.edits {
                                match e {
                                    lsp_types::OneOf::Left(text_edit) => {
                                        result.push(format!(
                                            "  L{}:{}-L{}:{}: {}",
                                            text_edit.range.start.line + 1,
                                            text_edit.range.start.character + 1,
                                            text_edit.range.end.line + 1,
                                            text_edit.range.end.character + 1,
                                            text_edit.new_text.replace('\n', "\\n")
                                        ));
                                    }
                                    lsp_types::OneOf::Right(annotated) => {
                                        result.push(format!(
                                            "  L{}:{}-L{}:{}: {}",
                                            annotated.text_edit.range.start.line + 1,
                                            annotated.text_edit.range.start.character + 1,
                                            annotated.text_edit.range.end.line + 1,
                                            annotated.text_edit.range.end.character + 1,
                                            annotated.text_edit.new_text.replace('\n', "\\n")
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if result.is_empty() {
        "No changes".to_string()
    } else {
        result.join("\n")
    }
}

fn format_completion(response: &CompletionResponse) -> String {
    let items: Vec<&CompletionItem> = match response {
        CompletionResponse::Array(items) => items.iter().collect(),
        CompletionResponse::List(list) => list.items.iter().collect(),
    };

    if items.is_empty() {
        return "No completions".to_string();
    }

    items
        .iter()
        .take(50)
        .map(|item| {
            let kind = item.kind.map(|k| format!(" [{k:?}]")).unwrap_or_default();
            let detail = item
                .detail
                .as_ref()
                .map(|d| format!(" - {d}"))
                .unwrap_or_default();
            format!("{}{}{}", item.label, kind, detail)
        })
        .collect::<Vec<_>>()
        .join("\n")
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

fn format_signature_help(help: &SignatureHelp) -> String {
    let mut result = Vec::new();

    for (i, sig) in help.signatures.iter().enumerate() {
        let active = if Some(u32::try_from(i).unwrap_or(u32::MAX)) == help.active_signature {
            " (active)"
        } else {
            ""
        };
        result.push(format!("{}. {}{}", i + 1, sig.label, active));

        if let Some(doc) = &sig.documentation {
            let doc_str = match doc {
                lsp_types::Documentation::String(s) => s.clone(),
                lsp_types::Documentation::MarkupContent(m) => m.value.clone(),
            };
            if !doc_str.is_empty() {
                result.push(format!("   {}", doc_str.lines().next().unwrap_or("")));
            }
        }

        if let Some(params) = &sig.parameters {
            for (j, param) in params.iter().enumerate() {
                let active_param =
                    if Some(u32::try_from(j).unwrap_or(u32::MAX)) == help.active_parameter {
                        " <--"
                    } else {
                        ""
                    };
                let label = match &param.label {
                    lsp_types::ParameterLabel::Simple(s) => s.clone(),
                    lsp_types::ParameterLabel::LabelOffsets([start, end]) => sig
                        .label
                        .chars()
                        .skip(*start as usize)
                        .take((*end - *start) as usize)
                        .collect(),
                };
                result.push(format!("   - {label}{active_param}"));
            }
        }
    }

    if result.is_empty() {
        "No signature information".to_string()
    } else {
        result.join("\n")
    }
}

fn format_text_edits(edits: &[TextEdit]) -> String {
    edits
        .iter()
        .map(|e| {
            format!(
                "L{}:{}-L{}:{}: {}",
                e.range.start.line + 1,
                e.range.start.character + 1,
                e.range.end.line + 1,
                e.range.end.character + 1,
                e.new_text.replace('\n', "\\n")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_incoming_calls(calls: &[CallHierarchyIncomingCall]) -> String {
    if calls.is_empty() {
        return "No incoming calls".to_string();
    }

    calls
        .iter()
        .map(|call| {
            let path = call.from.uri.path();
            let line = call.from.range.start.line + 1;
            let name = &call.from.name;
            let kind = format!("{:?}", call.from.kind);
            format!("{name} [{kind}] {path}:{line}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_outgoing_calls(calls: &[CallHierarchyOutgoingCall]) -> String {
    if calls.is_empty() {
        return "No outgoing calls".to_string();
    }

    calls
        .iter()
        .map(|call| {
            let path = call.to.uri.path();
            let line = call.to.range.start.line + 1;
            let name = &call.to.name;
            let kind = format!("{:?}", call.to.kind);
            format!("{name} [{kind}] {path}:{line}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_type_hierarchy_items(items: &[TypeHierarchyItem]) -> String {
    if items.is_empty() {
        return "No types found".to_string();
    }

    items
        .iter()
        .map(|item| {
            let path = item.uri.path();
            let line = item.range.start.line + 1;
            let kind = format!("{:?}", item.kind);
            format!("{} [{}] {}:{}", item.name, kind, path, line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use lsp_types::{
        DocumentSymbol, Range, SymbolInformation, SymbolKind, WorkspaceSymbolResponse,
    };

    fn make_position(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn make_range(start_line: u32, start_char: u32, end_line: u32, end_char: u32) -> Range {
        Range {
            start: make_position(start_line, start_char),
            end: make_position(end_line, end_char),
        }
    }

    fn make_document_symbol(name: &str, kind: SymbolKind, range: Range) -> DocumentSymbol {
        #[allow(
            deprecated,
            reason = "LSP spec uses deprecated fields in some versions"
        )]
        DocumentSymbol {
            name: name.to_string(),
            detail: None,
            kind,
            tags: None,
            deprecated: None,
            range,
            selection_range: range,
            children: None,
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

    #[test]
    fn test_find_symbol_exact_match_flat() -> Result<()> {
        let symbols = vec![
            make_symbol_info("foo", SymbolKind::FUNCTION, "file:///test.rs", 0)?,
            make_symbol_info("bar", SymbolKind::FUNCTION, "file:///test.rs", 10)?,
            make_symbol_info("baz", SymbolKind::STRUCT, "file:///test.rs", 20)?,
        ];
        let response = DocumentSymbolResponse::Flat(symbols);

        let result =
            find_symbol_in_document_response(&response, "bar").context("symbol not found")?;
        assert_eq!(result.line, 10);
        Ok(())
    }

    #[test]
    fn test_find_symbol_partial_match_flat() -> Result<()> {
        let symbols = vec![
            make_symbol_info("handle_request", SymbolKind::FUNCTION, "file:///test.rs", 5)?,
            make_symbol_info("process_data", SymbolKind::FUNCTION, "file:///test.rs", 15)?,
        ];
        let response = DocumentSymbolResponse::Flat(symbols);

        // Partial match "request"
        let result =
            find_symbol_in_document_response(&response, "request").context("symbol not found")?;
        assert_eq!(result.line, 5);
        Ok(())
    }

    #[test]
    fn test_find_symbol_exact_preferred_over_partial() -> Result<()> {
        let symbols = vec![
            make_symbol_info("foobar", SymbolKind::FUNCTION, "file:///test.rs", 0)?,
            make_symbol_info("foo", SymbolKind::FUNCTION, "file:///test.rs", 10)?,
        ];
        let response = DocumentSymbolResponse::Flat(symbols);

        // Exact match "foo" should be preferred over partial match "foobar"
        let result =
            find_symbol_in_document_response(&response, "foo").context("symbol not found")?;
        assert_eq!(result.line, 10);
        Ok(())
    }

    #[test]
    fn test_find_symbol_nested() -> Result<()> {
        let inner_symbol =
            make_document_symbol("inner_method", SymbolKind::METHOD, make_range(5, 0, 10, 0));
        let mut outer_symbol =
            make_document_symbol("MyClass", SymbolKind::CLASS, make_range(0, 0, 20, 0));
        outer_symbol.children = Some(vec![inner_symbol]);

        let response = DocumentSymbolResponse::Nested(vec![outer_symbol]);

        let result = find_symbol_in_document_response(&response, "inner_method")
            .context("symbol not found")?;
        assert_eq!(result.line, 5);
        Ok(())
    }

    #[test]
    fn test_find_symbol_nested_partial_match() -> Result<()> {
        let inner_symbol = make_document_symbol(
            "handle_request",
            SymbolKind::METHOD,
            make_range(15, 0, 20, 0),
        );
        let mut outer_symbol =
            make_document_symbol("Handler", SymbolKind::CLASS, make_range(0, 0, 30, 0));
        outer_symbol.children = Some(vec![inner_symbol]);

        let response = DocumentSymbolResponse::Nested(vec![outer_symbol]);

        // Partial match should find inner_method
        let result =
            find_symbol_in_document_response(&response, "request").context("symbol not found")?;
        assert_eq!(result.line, 15);
        Ok(())
    }

    #[test]
    fn test_find_symbol_not_found() -> Result<()> {
        let symbols = vec![make_symbol_info(
            "foo",
            SymbolKind::FUNCTION,
            "file:///test.rs",
            0,
        )?];
        let response = DocumentSymbolResponse::Flat(symbols);

        let result = find_symbol_in_document_response(&response, "nonexistent");
        assert!(result.is_none());
        Ok(())
    }

    #[test]
    fn test_find_workspace_symbol_exact_match() -> Result<()> {
        let symbols = vec![
            make_symbol_info("MyStruct", SymbolKind::STRUCT, "file:///src/lib.rs", 10)?,
            make_symbol_info("MyFunction", SymbolKind::FUNCTION, "file:///src/main.rs", 5)?,
        ];
        let response = WorkspaceSymbolResponse::Flat(symbols);

        let result =
            find_symbol_in_workspace_response(&response, "MyStruct").context("symbol not found")?;
        let (path, position): (std::path::PathBuf, _) = result;
        assert_eq!(path.to_string_lossy(), "/src/lib.rs");
        assert_eq!(position.line, 10);
        Ok(())
    }

    #[test]
    fn test_find_workspace_symbol_partial_match() -> Result<()> {
        let symbols = vec![make_symbol_info(
            "LspBridgeHandler",
            SymbolKind::STRUCT,
            "file:///src/handler.rs",
            50,
        )?];
        let response = WorkspaceSymbolResponse::Flat(symbols);

        let result =
            find_symbol_in_workspace_response(&response, "Bridge").context("symbol not found")?;
        let (path, position): (std::path::PathBuf, _) = result;
        assert_eq!(path.to_string_lossy(), "/src/handler.rs");
        assert_eq!(position.line, 50);
        Ok(())
    }

    #[test]
    fn test_find_references_input_validation() -> Result<()> {
        // Test that FindReferencesInput can be deserialized with symbol
        let json = serde_json::json!({
            "symbol": "MyStruct"
        });
        let input: FindReferencesInput = serde_json::from_value(json)?;
        assert_eq!(input.symbol, Some("MyStruct".to_string()));
        assert!(input.file.is_none());
        assert!(input.line.is_none());
        assert!(input.character.is_none());
        assert!(input.include_declaration); // default true

        // Test with position
        let json = serde_json::json!({
            "file": "/path/to/file.rs",
            "line": 10,
            "character": 5
        });
        let input: FindReferencesInput = serde_json::from_value(json)?;
        assert!(input.symbol.is_none());
        assert_eq!(input.file, Some("/path/to/file.rs".to_string()));
        assert_eq!(input.line, Some(10));
        assert_eq!(input.character, Some(5));

        // Test with both symbol and file (to narrow scope)
        let json = serde_json::json!({
            "symbol": "my_function",
            "file": "/path/to/file.rs"
        });
        let input: FindReferencesInput = serde_json::from_value(json)?;
        assert_eq!(input.symbol, Some("my_function".to_string()));
        assert_eq!(input.file, Some("/path/to/file.rs".to_string()));
        Ok(())
    }
}

//! Bridge handler that maps MCP tool calls to LSP requests.

use anyhow::{Result, anyhow};
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyOutgoingCall,
    CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams, CodeActionContext,
    CodeActionOrCommand, CodeActionParams, CompletionItem, CompletionParams, CompletionResponse,
    Diagnostic, DiagnosticSeverity, DocumentChanges, DocumentFormattingParams,
    DocumentRangeFormattingParams, DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse,
    FormattingOptions, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, Location,
    LocationLink, Position, PositionEncodingKind, Range, ReferenceContext, ReferenceParams,
    RenameParams, SignatureHelp, SignatureHelpParams, SymbolInformation, TextDocumentIdentifier,
    TextDocumentPositionParams, TextEdit, TypeHierarchyItem, TypeHierarchyPrepareParams,
    TypeHierarchySubtypesParams, TypeHierarchySupertypesParams, WorkspaceEdit,
    WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::lsp::LspClient;
use crate::mcp::{CallToolResult, Tool, ToolHandler};

use super::{DocumentManager, DocumentNotification};

/// Input for tools that need file + position.
#[derive(Debug, Deserialize)]
pub struct PositionInput {
    pub file: String,
    pub line: u32,
    pub character: u32,
}

/// Input for tools that need only a file path.
#[derive(Debug, Deserialize)]
pub struct FileInput {
    pub file: String,
}

/// Input for references tool.
#[derive(Debug, Deserialize)]
pub struct ReferencesInput {
    pub file: String,
    pub line: u32,
    pub character: u32,
    #[serde(default = "default_true")]
    pub include_declaration: bool,
}

fn default_true() -> bool {
    true
}

/// Input for workspace symbol search.
#[derive(Debug, Deserialize)]
pub struct WorkspaceSymbolInput {
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
    #[serde(default = "default_true")]
    pub dry_run: bool,
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

fn default_tab_size() -> u32 {
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
    pub file: String,
    pub line: u32,
    pub character: u32,
    /// "incoming" or "outgoing"
    pub direction: String,
}

/// Input for type hierarchy.
#[derive(Debug, Deserialize)]
pub struct TypeHierarchyInput {
    pub file: String,
    pub line: u32,
    pub character: u32,
    /// "supertypes" or "subtypes"
    pub direction: String,
}

/// Bridge handler that implements MCP ToolHandler trait.
pub struct LspBridgeHandler {
    client: Arc<Mutex<LspClient>>,
    doc_manager: Arc<Mutex<DocumentManager>>,
    runtime: Handle,
}

impl LspBridgeHandler {
    pub fn new(
        client: Arc<Mutex<LspClient>>,
        doc_manager: Arc<Mutex<DocumentManager>>,
        runtime: Handle,
    ) -> Self {
        Self {
            client,
            doc_manager,
            runtime,
        }
    }

    /// Checks if the LSP server is still alive.
    fn check_alive(&self) -> Result<()> {
        let alive = self.runtime.block_on(async {
            let client = self.client.lock().await;
            client.is_alive()
        });
        if !alive {
            Err(anyhow!("LSP server is no longer running"))
        } else {
            Ok(())
        }
    }

    /// Ensures a document is open and synced with the LSP server.
    async fn ensure_document_open(&self, path: &Path) -> Result<lsp_types::Uri> {
        let mut doc_manager = self.doc_manager.lock().await;
        let client = self.client.lock().await;

        // Check if LSP is still alive
        if !client.is_alive() {
            return Err(anyhow!("LSP server is no longer running"));
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

        doc_manager.uri_for_path(path)
    }

    fn parse_position_input(&self, arguments: Option<serde_json::Value>) -> Result<PositionInput> {
        serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
            .map_err(|e| anyhow!("Invalid arguments: {}", e))
    }

    fn validate_absolute_path(&self, file: &str) -> Result<PathBuf> {
        let path = PathBuf::from(file);
        if !path.is_absolute() {
            return Err(anyhow!("File path must be absolute: {}", file));
        }
        Ok(path)
    }

    fn handle_hover(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        let input = self.parse_position_input(arguments)?;
        let path = self.validate_absolute_path(&input.file)?;

        debug!(
            "Hover request: {}:{}:{}",
            input.file, input.line, input.character
        );

        let result = self.runtime.block_on(async {
            let uri = self.ensure_document_open(&path).await?;
            let params = HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position {
                        line: input.line,
                        character: input.character,
                    },
                },
                work_done_progress_params: Default::default(),
            };
            let client = self.client.lock().await;
            client.hover(params).await
        })?;

        match result {
            Some(hover) => Ok(CallToolResult::text(format_hover(&hover))),
            None => Ok(CallToolResult::text("No hover information available")),
        }
    }

    fn handle_definition(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        let input = self.parse_position_input(arguments)?;
        let path = self.validate_absolute_path(&input.file)?;

        debug!(
            "Definition request: {}:{}:{}",
            input.file, input.line, input.character
        );

        let result = self.runtime.block_on(async {
            let uri = self.ensure_document_open(&path).await?;
            let params = GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position {
                        line: input.line,
                        character: input.character,
                    },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let client = self.client.lock().await;
            client.definition(params).await
        })?;

        match result {
            Some(response) => Ok(CallToolResult::text(format_definition_response(&response))),
            None => Ok(CallToolResult::text("No definition found")),
        }
    }

    fn handle_type_definition(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input = self.parse_position_input(arguments)?;
        let path = self.validate_absolute_path(&input.file)?;

        debug!(
            "Type definition request: {}:{}:{}",
            input.file, input.line, input.character
        );

        let result = self.runtime.block_on(async {
            let uri = self.ensure_document_open(&path).await?;
            let params = GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position {
                        line: input.line,
                        character: input.character,
                    },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let client = self.client.lock().await;
            client.type_definition(params).await
        })?;

        match result {
            Some(response) => Ok(CallToolResult::text(format_definition_response(&response))),
            None => Ok(CallToolResult::text("No type definition found")),
        }
    }

    fn handle_implementation(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input = self.parse_position_input(arguments)?;
        let path = self.validate_absolute_path(&input.file)?;

        debug!(
            "Implementation request: {}:{}:{}",
            input.file, input.line, input.character
        );

        let result = self.runtime.block_on(async {
            let uri = self.ensure_document_open(&path).await?;
            let params = GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position {
                        line: input.line,
                        character: input.character,
                    },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let client = self.client.lock().await;
            client.implementation(params).await
        })?;

        match result {
            Some(response) => Ok(CallToolResult::text(format_definition_response(&response))),
            None => Ok(CallToolResult::text("No implementations found")),
        }
    }

    fn handle_references(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        let input: ReferencesInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {}", e))?;

        let path = self.validate_absolute_path(&input.file)?;

        debug!(
            "References request: {}:{}:{}",
            input.file, input.line, input.character
        );

        let result = self.runtime.block_on(async {
            let uri = self.ensure_document_open(&path).await?;
            let params = ReferenceParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position {
                        line: input.line,
                        character: input.character,
                    },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: ReferenceContext {
                    include_declaration: input.include_declaration,
                },
            };
            let client = self.client.lock().await;
            client.references(params).await
        })?;

        match result {
            Some(locations) if !locations.is_empty() => {
                Ok(CallToolResult::text(format_locations(&locations)))
            }
            _ => Ok(CallToolResult::text("No references found")),
        }
    }

    fn handle_document_symbols(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input: FileInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {}", e))?;

        let path = self.validate_absolute_path(&input.file)?;

        debug!("Document symbols request: {}", input.file);

        let result = self.runtime.block_on(async {
            let uri = self.ensure_document_open(&path).await?;
            let params = DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let client = self.client.lock().await;
            client.document_symbols(params).await
        })?;

        match result {
            Some(response) => Ok(CallToolResult::text(format_document_symbols(&response))),
            None => Ok(CallToolResult::text("No symbols found")),
        }
    }

    fn handle_workspace_symbols(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        // Check LSP health first (this handler doesn't open a document)
        self.check_alive()?;

        let input: WorkspaceSymbolInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {}", e))?;

        debug!("Workspace symbols request: query={}", input.query);

        let result = self.runtime.block_on(async {
            let params = WorkspaceSymbolParams {
                query: input.query,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let client = self.client.lock().await;
            client.workspace_symbols(params).await
        })?;

        match result {
            Some(response) => Ok(CallToolResult::text(format_workspace_symbols(&response))),
            None => Ok(CallToolResult::text("No symbols found")),
        }
    }

    fn handle_code_actions(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        let input: CodeActionInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {}", e))?;

        let path = self.validate_absolute_path(&input.file)?;

        debug!(
            "Code actions request: {} [{},{}]-[{},{}]",
            input.file,
            input.start_line,
            input.start_character,
            input.end_line,
            input.end_character
        );

        let result = self.runtime.block_on(async {
            let uri = self.ensure_document_open(&path).await?;

            // Get diagnostics for the range to include in context
            let client = self.client.lock().await;
            let diagnostics = client.get_diagnostics(&uri).await;

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
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            client.code_actions(params).await
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
                .map_err(|e| anyhow!("Invalid arguments: {}", e))?;

        let path = self.validate_absolute_path(&input.file)?;

        debug!(
            "Rename request: {}:{}:{} -> {} (dry_run: {})",
            input.file, input.line, input.character, input.new_name, input.dry_run
        );

        let result = self.runtime.block_on(async {
            let uri = self.ensure_document_open(&path).await?;
            let params = RenameParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position {
                        line: input.line,
                        character: input.character,
                    },
                },
                new_name: input.new_name,
                work_done_progress_params: Default::default(),
            };
            let client = self.client.lock().await;
            client.rename(params).await
        })?;

        match result {
            Some(edit) => {
                let diff_text = format_workspace_edit(&edit);
                if input.dry_run {
                    Ok(CallToolResult::text(diff_text))
                } else {
                    // Get encoding from client
                    let encoding = self.runtime.block_on(async {
                        let client = self.client.lock().await;
                        client.encoding()
                    });

                    // Apply changes
                    self.runtime.block_on(async {
                        apply_workspace_edit(&edit, encoding).await
                    })?;
                    Ok(CallToolResult::text(format!(
                        "Successfully applied rename. Changes:\n{}",
                        diff_text
                    )))
                }
            }
            None => Ok(CallToolResult::text(
                "Rename not supported at this location",
            )),
        }
    }

    fn handle_completion(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        let input = self.parse_position_input(arguments)?;
        let path = self.validate_absolute_path(&input.file)?;

        debug!(
            "Completion request: {}:{}:{}",
            input.file, input.line, input.character
        );

        let result = self.runtime.block_on(async {
            let uri = self.ensure_document_open(&path).await?;
            let params = CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position {
                        line: input.line,
                        character: input.character,
                    },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            };
            let client = self.client.lock().await;
            client.completion(params).await
        })?;

        match result {
            Some(response) => Ok(CallToolResult::text(format_completion(&response))),
            None => Ok(CallToolResult::text("No completions available")),
        }
    }

    fn handle_diagnostics(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        let input: FileInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {}", e))?;

        let path = self.validate_absolute_path(&input.file)?;

        debug!("Diagnostics request: {}", input.file);

        let diagnostics = self.runtime.block_on(async {
            let uri = self.ensure_document_open(&path).await?;
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            let client = self.client.lock().await;
            Ok::<_, anyhow::Error>(client.get_diagnostics(&uri).await)
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
        let input = self.parse_position_input(arguments)?;
        let path = self.validate_absolute_path(&input.file)?;

        debug!(
            "Signature help request: {}:{}:{}",
            input.file, input.line, input.character
        );

        let result = self.runtime.block_on(async {
            let uri = self.ensure_document_open(&path).await?;
            let params = SignatureHelpParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position {
                        line: input.line,
                        character: input.character,
                    },
                },
                work_done_progress_params: Default::default(),
                context: None,
            };
            let client = self.client.lock().await;
            client.signature_help(params).await
        })?;

        match result {
            Some(help) => Ok(CallToolResult::text(format_signature_help(&help))),
            None => Ok(CallToolResult::text("No signature help available")),
        }
    }

    fn handle_formatting(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        let input: FormattingInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {}", e))?;

        let path = self.validate_absolute_path(&input.file)?;

        debug!("Formatting request: {}", input.file);

        let result = self.runtime.block_on(async {
            let uri = self.ensure_document_open(&path).await?;
            let params = DocumentFormattingParams {
                text_document: TextDocumentIdentifier { uri },
                options: FormattingOptions {
                    tab_size: input.tab_size,
                    insert_spaces: input.insert_spaces,
                    ..Default::default()
                },
                work_done_progress_params: Default::default(),
            };
            let client = self.client.lock().await;
            client.formatting(params).await
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
                .map_err(|e| anyhow!("Invalid arguments: {}", e))?;

        let path = self.validate_absolute_path(&input.file)?;

        debug!(
            "Range formatting request: {} [{},{}]-[{},{}]",
            input.file,
            input.start_line,
            input.start_character,
            input.end_line,
            input.end_character
        );

        let result = self.runtime.block_on(async {
            let uri = self.ensure_document_open(&path).await?;
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
                    ..Default::default()
                },
                work_done_progress_params: Default::default(),
            };
            let client = self.client.lock().await;
            client.range_formatting(params).await
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
                .map_err(|e| anyhow!("Invalid arguments: {}", e))?;

        let path = self.validate_absolute_path(&input.file)?;

        debug!(
            "Call hierarchy request: {}:{}:{} direction={}",
            input.file, input.line, input.character, input.direction
        );

        let result = self.runtime.block_on(async {
            let uri = self.ensure_document_open(&path).await?;

            // First, prepare the call hierarchy
            let prepare_params = CallHierarchyPrepareParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position {
                        line: input.line,
                        character: input.character,
                    },
                },
                work_done_progress_params: Default::default(),
            };

            let client = self.client.lock().await;
            let items = client.prepare_call_hierarchy(prepare_params).await?;

            let Some(items) = items else {
                return Ok::<_, anyhow::Error>(None);
            };

            if items.is_empty() {
                return Ok(None);
            }

            // Get calls for the first item
            let item = items.into_iter().next().unwrap();

            match input.direction.as_str() {
                "incoming" => {
                    let params = CallHierarchyIncomingCallsParams {
                        item,
                        work_done_progress_params: Default::default(),
                        partial_result_params: Default::default(),
                    };
                    let calls = client.incoming_calls(params).await?;
                    Ok(calls.map(|c| format_incoming_calls(&c)))
                }
                "outgoing" => {
                    let params = CallHierarchyOutgoingCallsParams {
                        item,
                        work_done_progress_params: Default::default(),
                        partial_result_params: Default::default(),
                    };
                    let calls = client.outgoing_calls(params).await?;
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
                .map_err(|e| anyhow!("Invalid arguments: {}", e))?;

        let path = self.validate_absolute_path(&input.file)?;

        debug!(
            "Type hierarchy request: {}:{}:{} direction={}",
            input.file, input.line, input.character, input.direction
        );

        let result = self.runtime.block_on(async {
            let uri = self.ensure_document_open(&path).await?;

            // First, prepare the type hierarchy
            let prepare_params = TypeHierarchyPrepareParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position {
                        line: input.line,
                        character: input.character,
                    },
                },
                work_done_progress_params: Default::default(),
            };

            let client = self.client.lock().await;
            let items = client.prepare_type_hierarchy(prepare_params).await?;

            let Some(items) = items else {
                return Ok::<_, anyhow::Error>(None);
            };

            if items.is_empty() {
                return Ok(None);
            }

            // Get hierarchy for the first item
            let item = items.into_iter().next().unwrap();

            match input.direction.as_str() {
                "supertypes" => {
                    let params = TypeHierarchySupertypesParams {
                        item,
                        work_done_progress_params: Default::default(),
                        partial_result_params: Default::default(),
                    };
                    let types = client.supertypes(params).await?;
                    Ok(types.map(|t| format_type_hierarchy_items(&t)))
                }
                "subtypes" => {
                    let params = TypeHierarchySubtypesParams {
                        item,
                        work_done_progress_params: Default::default(),
                        partial_result_params: Default::default(),
                    };
                    let types = client.subtypes(params).await?;
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
}

impl ToolHandler for LspBridgeHandler {
    fn list_tools(&self) -> Vec<Tool> {
        vec![
            Tool {
                name: "lsp_hover".to_string(),
                description: Some("Get hover information (documentation, type info) for a symbol at a position.".to_string()),
                input_schema: position_schema(),
            },
            Tool {
                name: "lsp_definition".to_string(),
                description: Some("Go to the definition of a symbol.".to_string()),
                input_schema: position_schema(),
            },
            Tool {
                name: "lsp_type_definition".to_string(),
                description: Some("Go to the type definition of a symbol (e.g., for a variable, go to its type's definition).".to_string()),
                input_schema: position_schema(),
            },
            Tool {
                name: "lsp_implementation".to_string(),
                description: Some("Find implementations of an interface, trait, or abstract method.".to_string()),
                input_schema: position_schema(),
            },
            Tool {
                name: "lsp_references".to_string(),
                description: Some("Find all references to a symbol across the codebase.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Absolute path to the file" },
                        "line": { "type": "integer", "description": "Line number (0-indexed)" },
                        "character": { "type": "integer", "description": "Character position (0-indexed)" },
                        "include_declaration": { "type": "boolean", "description": "Include the declaration in results (default: true)" }
                    },
                    "required": ["file", "line", "character"]
                }),
            },
            Tool {
                name: "lsp_document_symbols".to_string(),
                description: Some("Get the symbol outline of a file (functions, classes, variables, etc.).".to_string()),
                input_schema: file_schema(),
            },
            Tool {
                name: "lsp_workspace_symbols".to_string(),
                description: Some("Search for symbols across the entire workspace by name.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search query for symbol names" }
                    },
                    "required": ["query"]
                }),
            },
            Tool {
                name: "lsp_code_actions".to_string(),
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
                name: "lsp_rename".to_string(),
                description: Some("Compute the edits needed to rename a symbol across the codebase. Returns a list of changes. If dry_run is false, applies the changes.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Absolute path to the file" },
                        "line": { "type": "integer", "description": "Line number (0-indexed)" },
                        "character": { "type": "integer", "description": "Character position (0-indexed)" },
                        "new_name": { "type": "string", "description": "New name for the symbol" },
                        "dry_run": { "type": "boolean", "description": "If true (default), only return expected changes. If false, apply changes to disk." }
                    },
                    "required": ["file", "line", "character", "new_name"]
                }),
            },
            Tool {
                name: "lsp_completion".to_string(),
                description: Some("Get completion suggestions at a position.".to_string()),
                input_schema: position_schema(),
            },
            Tool {
                name: "lsp_diagnostics".to_string(),
                description: Some("Get diagnostics (errors, warnings, hints) for a file.".to_string()),
                input_schema: file_schema(),
            },
            Tool {
                name: "lsp_signature_help".to_string(),
                description: Some("Get function signature help at a position (parameter info while typing a call).".to_string()),
                input_schema: position_schema(),
            },
            Tool {
                name: "lsp_formatting".to_string(),
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
                name: "lsp_range_formatting".to_string(),
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
                name: "lsp_call_hierarchy".to_string(),
                description: Some("Get incoming or outgoing calls for a function/method.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Absolute path to the file" },
                        "line": { "type": "integer", "description": "Line number (0-indexed)" },
                        "character": { "type": "integer", "description": "Character position (0-indexed)" },
                        "direction": { "type": "string", "enum": ["incoming", "outgoing"], "description": "Direction: 'incoming' (who calls this?) or 'outgoing' (what does this call?)" }
                    },
                    "required": ["file", "line", "character", "direction"]
                }),
            },
            Tool {
                name: "lsp_type_hierarchy".to_string(),
                description: Some("Get supertypes or subtypes of a type.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Absolute path to the file" },
                        "line": { "type": "integer", "description": "Line number (0-indexed)" },
                        "character": { "type": "integer", "description": "Character position (0-indexed)" },
                        "direction": { "type": "string", "enum": ["supertypes", "subtypes"], "description": "Direction: 'supertypes' (parent types) or 'subtypes' (child types)" }
                    },
                    "required": ["file", "line", "character", "direction"]
                }),
            },
        ]
    }

    fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        match name {
            "lsp_hover" => self.handle_hover(arguments),
            "lsp_definition" => self.handle_definition(arguments),
            "lsp_type_definition" => self.handle_type_definition(arguments),
            "lsp_implementation" => self.handle_implementation(arguments),
            "lsp_references" => self.handle_references(arguments),
            "lsp_document_symbols" => self.handle_document_symbols(arguments),
            "lsp_workspace_symbols" => self.handle_workspace_symbols(arguments),
            "lsp_code_actions" => self.handle_code_actions(arguments),
            "lsp_rename" => self.handle_rename(arguments),
            "lsp_completion" => self.handle_completion(arguments),
            "lsp_diagnostics" => self.handle_diagnostics(arguments),
            "lsp_signature_help" => self.handle_signature_help(arguments),
            "lsp_formatting" => self.handle_formatting(arguments),
            "lsp_range_formatting" => self.handle_range_formatting(arguments),
            "lsp_call_hierarchy" => self.handle_call_hierarchy(arguments),
            "lsp_type_hierarchy" => self.handle_type_hierarchy(arguments),
            _ => Err(anyhow!("Unknown tool: {}", name)),
        }
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

fn file_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file": { "type": "string", "description": "Absolute path to the file" }
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

fn format_location(location: &Location) -> String {
    let path = location.uri.path();
    let line = location.range.start.line + 1;
    let col = location.range.start.character + 1;
    format!("{}:{}:{}", path, line, col)
}

fn format_location_link(link: &LocationLink) -> String {
    let path = link.target_uri.path();
    let line = link.target_range.start.line + 1;
    let col = link.target_range.start.character + 1;
    format!("{}:{}:{}", path, line, col)
}

fn format_locations(locations: &[Location]) -> String {
    locations
        .iter()
        .map(format_location)
        .collect::<Vec<_>>()
        .join("\n")
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
                            result.push(format!("Operation: {:?}", resource_op));
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
            let kind = item.kind.map(|k| format!(" [{:?}]", k)).unwrap_or_default();
            let detail = item
                .detail
                .as_ref()
                .map(|d| format!(" - {}", d))
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
        let active = if Some(i as u32) == help.active_signature {
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
                let active_param = if Some(j as u32) == help.active_parameter {
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
                result.push(format!("   - {}{}", label, active_param));
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
            format!("{} [{}] {}:{}", name, kind, path, line)
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
            format!("{} [{}] {}:{}", name, kind, path, line)
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

async fn apply_workspace_edit(edit: &WorkspaceEdit, encoding: PositionEncodingKind) -> Result<()> {
    // Collect all edits by file path
    let mut file_edits: HashMap<PathBuf, Vec<TextEdit>> = HashMap::new();

    if let Some(changes) = &edit.changes {
        for (uri, edits) in changes {
            let url = url::Url::parse(uri.as_str())
                .map_err(|_| anyhow!("Invalid URI: {}", uri.as_str()))?;
            let path = url
                .to_file_path()
                .map_err(|_| anyhow!("Invalid file URI: {}", uri.as_str()))?;
            file_edits.entry(path).or_default().extend(edits.iter().cloned());
        }
    }

    if let Some(doc_changes) = &edit.document_changes {
        match doc_changes {
            DocumentChanges::Edits(edits) => {
                for edit in edits {
                    let uri = &edit.text_document.uri;
                    let url = url::Url::parse(uri.as_str())
                        .map_err(|_| anyhow!("Invalid URI: {}", uri.as_str()))?;
                    let path = url
                        .to_file_path()
                        .map_err(|_| anyhow!("Invalid file URI: {}", uri.as_str()))?;
                    let changes = edit
                        .edits
                        .iter()
                        .map(|e| match e {
                            lsp_types::OneOf::Left(te) => te.clone(),
                            lsp_types::OneOf::Right(ae) => annotated_text_edit_to_text_edit(ae),
                        });
                    file_edits.entry(path).or_default().extend(changes);
                }
            }
            DocumentChanges::Operations(ops) => {
                // TODO: Support create/rename/delete operations
                // For now, we only support edits within operations if they map simply
                warn!("DocumentChange operations (create/rename/delete) are not yet fully supported. Only text edits will be applied.");
                for op in ops {
                    if let lsp_types::DocumentChangeOperation::Edit(edit) = op {
                        let uri = &edit.text_document.uri;
                        let url = url::Url::parse(uri.as_str())
                            .map_err(|_| anyhow!("Invalid URI: {}", uri.as_str()))?;
                        let path = url
                            .to_file_path()
                            .map_err(|_| anyhow!("Invalid file URI: {}", uri.as_str()))?;
                        let changes = edit.edits.iter().map(|e| match e {
                            lsp_types::OneOf::Left(te) => te.clone(),
                            lsp_types::OneOf::Right(ae) => annotated_text_edit_to_text_edit(ae),
                        });
                        file_edits.entry(path).or_default().extend(changes);
                    }
                }
            }
        }
    }

    // Apply edits for each file
    for (path, edits) in file_edits {
        apply_edits_to_file(&path, edits, encoding.clone()).await?;
    }

    Ok(())
}

fn annotated_text_edit_to_text_edit(
    annotated: &lsp_types::AnnotatedTextEdit,
) -> TextEdit {
    TextEdit {
        range: annotated.text_edit.range,
        new_text: annotated.text_edit.new_text.clone(),
    }
}

async fn apply_edits_to_file(path: &Path, mut edits: Vec<TextEdit>, encoding: PositionEncodingKind) -> Result<()> {
    let content = fs::read_to_string(path).await?;

    // Sort edits by start position descending to apply from bottom up
    edits.sort_by(|a, b| {
        b.range
            .start
            .line
            .cmp(&a.range.start.line)
            .then(b.range.start.character.cmp(&a.range.start.character))
    });

    let mut result = content.clone();

    for edit in edits {
        let start_offset = position_to_offset(&content, edit.range.start, &encoding)?;
        let end_offset = position_to_offset(&content, edit.range.end, &encoding)?;

        if start_offset > end_offset {
            return Err(anyhow!(
                "Invalid range: start {} > end {}",
                start_offset,
                end_offset
            ));
        }

        result.replace_range(start_offset..end_offset, &edit.new_text);
    }

    fs::write(path, result).await?;
    Ok(())
}

fn position_to_offset(content: &str, position: Position, encoding: &PositionEncodingKind) -> Result<usize> {
    let mut current_line = 0;
    let mut line_start_byte = 0;

    // Find the start of the target line
    if position.line > 0 {
        let mut lines_found = 0;
        for (i, b) in content.as_bytes().iter().enumerate() {
            if *b == b'\n' {
                lines_found += 1;
                if lines_found == position.line {
                    line_start_byte = i + 1;
                    current_line = lines_found;
                    break;
                }
            }
        }
        
        if current_line != position.line {
            return Err(anyhow!("Line {} out of bounds", position.line));
        }
    }

    let line_content = &content[line_start_byte..];
    let line_end_byte = line_content.find('\n').map(|i| line_start_byte + i).unwrap_or(content.len());
    let line_text = &content[line_start_byte..line_end_byte];

    if *encoding == PositionEncodingKind::UTF8 {
        // Character is a byte offset
        let char_offset = position.character as usize;
        if char_offset <= line_text.len() {
            Ok(line_start_byte + char_offset)
        } else {
            Err(anyhow!("Character offset {} out of bounds for line {}", char_offset, position.line))
        }
    } else {
        // Default to UTF-16 logic
        // Character is a UTF-16 code unit offset
        let mut utf16_offset = 0;
        let mut byte_offset = 0;
        
        for c in line_text.chars() {
            if utf16_offset >= position.character as usize {
                break;
            }
            utf16_offset += c.len_utf16();
            byte_offset += c.len_utf8();
        }
        
        if utf16_offset == position.character as usize {
            Ok(line_start_byte + byte_offset)
        } else {
            Err(anyhow!("Position {:?} lands in the middle of a UTF-16 surrogate pair or out of bounds", position))
        }
    }
}

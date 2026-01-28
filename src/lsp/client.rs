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

use anyhow::{Context, Result, anyhow};
use bytes::BytesMut;
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    ClientCapabilities, CodeActionParams, CodeActionResponse, CompletionParams, CompletionResponse,
    Diagnostic, DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DocumentFormattingParams, DocumentRangeFormattingParams, DocumentSymbolParams,
    DocumentSymbolResponse, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams,
    InitializeParams, InitializeResult, InitializedParams, PositionEncodingKind,
    PublishDiagnosticsParams, ReferenceParams, RenameParams, SignatureHelp, SignatureHelpParams,
    TextEdit, TypeHierarchyItem, TypeHierarchyPrepareParams, TypeHierarchySubtypesParams,
    TypeHierarchySupertypesParams, Uri, WorkspaceEdit, WorkspaceFolder, WorkspaceSymbolParams,
    WorkspaceSymbolResponse,
};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, oneshot};
use tracing::{debug, error, trace, warn};

use super::protocol::{self, NotificationMessage, RequestId, RequestMessage, ResponseMessage};

/// Cached diagnostics for a file.
pub type DiagnosticsCache = Arc<Mutex<HashMap<Uri, Vec<Diagnostic>>>>;

/// Default timeout for LSP requests.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Manages communication with an LSP server process.
pub struct LspClient {
    next_id: AtomicI64,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<ResponseMessage>>>>,
    diagnostics: DiagnosticsCache,
    alive: Arc<AtomicBool>,
    encoding: PositionEncodingKind,
    _reader_handle: tokio::task::JoinHandle<()>,
    _child: Child,
}

impl LspClient {
    /// Spawns the LSP server process and starts the response reader task.
    pub async fn spawn(program: &str, args: &[&str]) -> Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("Failed to spawn LSP server: {}", program))?;

        let stdin = child.stdin.take().expect("stdin not captured");
        let stdout = child.stdout.take().expect("stdout not captured");

        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<ResponseMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let diagnostics: DiagnosticsCache = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));

        let reader_handle = tokio::spawn(Self::reader_task(
            stdin.clone(),
            stdout,
            pending.clone(),
            diagnostics.clone(),
            alive.clone(),
        ));

        Ok(Self {
            next_id: AtomicI64::new(1),
            stdin,
            pending,
            diagnostics,
            alive,
            encoding: PositionEncodingKind::UTF16, // Default per spec
            _reader_handle: reader_handle,
            _child: child,
        })
    }

    /// Background task that reads LSP messages and routes responses to pending requests.
    async fn reader_task(
        stdin: Arc<Mutex<ChildStdin>>,
        stdout: ChildStdout,
        pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<ResponseMessage>>>>,
        diagnostics: DiagnosticsCache,
        alive: Arc<AtomicBool>,
    ) {
        let mut reader = BufReader::new(stdout);
        let mut buffer = BytesMut::with_capacity(8192);

        loop {
            // Read more data into buffer
            let mut temp = [0u8; 4096];
            match reader.read(&mut temp).await {
                Ok(0) => {
                    debug!("LSP stdout closed");
                    break;
                }
                Ok(n) => {
                    buffer.extend_from_slice(&temp[..n]);
                }
                Err(e) => {
                    error!("Error reading from LSP stdout: {}", e);
                    break;
                }
            }

            // Try to parse complete messages
            while let Ok(Some(message_str)) = protocol::try_parse_message(&mut buffer) {
                trace!("Received LSP message: {}", message_str);

                let value: serde_json::Value = match serde_json::from_str(&message_str) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Failed to parse JSON: {}", e);
                        continue;
                    }
                };

                // Check message type
                if let Some(method) = value.get("method").and_then(|m| m.as_str()) {
                    // Request or Notification
                    if let Some(id) = value.get("id") {
                        // Server Request (e.g., workspace/configuration)
                        debug!("Received server request: {} (id: {})", method, id);

                        // Reply with MethodNotFound to unblock server
                        let response = ResponseMessage {
                            jsonrpc: "2.0".to_string(),
                            id: Some(
                                serde_json::from_value(id.clone()).unwrap_or(RequestId::Number(0)),
                            ),
                            result: None,
                            error: Some(protocol::ResponseError {
                                code: -32601, // MethodNotFound
                                message: format!("Method '{}' not supported by client", method),
                                data: None,
                            }),
                        };

                        if let Ok(body) = serde_json::to_string(&response) {
                            let header = format!("Content-Length: {}\r\n\r\n", body.len());
                            let mut stdin_guard = stdin.lock().await;
                            if let Err(e) = stdin_guard.write_all(header.as_bytes()).await {
                                warn!("Failed to write response header: {}", e);
                            } else if let Err(e) = stdin_guard.write_all(body.as_bytes()).await {
                                warn!("Failed to write response body: {}", e);
                            } else if let Err(e) = stdin_guard.flush().await {
                                warn!("Failed to flush response: {}", e);
                            }
                        }
                    } else {
                        // Notification
                        if let Ok(notification) =
                            serde_json::from_value::<NotificationMessage>(value)
                        {
                            Self::handle_notification(&notification, &diagnostics).await;
                        }
                    }
                } else if value.get("id").is_some() {
                    // Response
                    if let Ok(response) = serde_json::from_value::<ResponseMessage>(value) {
                        if let Some(id) = &response.id {
                            let mut pending = pending.lock().await;
                            if let Some(sender) = pending.remove(id) {
                                let _ = sender.send(response);
                            } else {
                                warn!("Received response for unknown request id: {:?}", id);
                            }
                        }
                    }
                } else {
                    warn!("Unknown message format: {}", message_str);
                }
            }
        }

        // Mark server as dead
        alive.store(false, Ordering::SeqCst);
        warn!("LSP reader task exiting - server connection lost");
    }

    /// Handles incoming LSP notifications.
    async fn handle_notification(
        notification: &NotificationMessage,
        diagnostics: &DiagnosticsCache,
    ) {
        match notification.method.as_str() {
            "textDocument/publishDiagnostics" => {
                if let Ok(params) =
                    serde_json::from_value::<PublishDiagnosticsParams>(notification.params.clone())
                {
                    debug!(
                        "Received {} diagnostics for {:?}",
                        params.diagnostics.len(),
                        params.uri.as_str()
                    );
                    let mut cache = diagnostics.lock().await;
                    cache.insert(params.uri, params.diagnostics);
                } else {
                    warn!("Failed to parse publishDiagnostics params");
                }
            }
            "window/logMessage" | "window/showMessage" => {
                // Log messages from the server
                if let Some(message) = notification.params.get("message").and_then(|m| m.as_str()) {
                    debug!("LSP server message: {}", message);
                }
            }
            _ => {
                trace!(
                    "Ignoring notification: {} params={}",
                    notification.method, notification.params
                );
            }
        }
    }

    /// Sends a request and waits for the response with timeout.
    async fn request<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R> {
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::SeqCst));

        let request = RequestMessage {
            jsonrpc: "2.0".to_string(),
            id: id.clone(),
            method: method.to_string(),
            params: serde_json::to_value(params)?,
        };

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id.clone(), tx);
        }

        self.send_message(&request).await?;

        // Wait for response with timeout
        let response = match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => {
                // Channel closed - server died
                return Err(anyhow!("LSP server closed connection"));
            }
            Err(_) => {
                // Timeout - clean up pending request
                let mut pending = self.pending.lock().await;
                pending.remove(&id);
                return Err(anyhow!(
                    "LSP request '{}' timed out after {:?}",
                    method,
                    REQUEST_TIMEOUT
                ));
            }
        };

        if let Some(error) = response.error {
            return Err(anyhow!("LSP error {}: {}", error.code, error.message));
        }

        // Handle null/missing result - use JSON null as default
        let result = response.result.unwrap_or(serde_json::Value::Null);

        serde_json::from_value(result).context("Failed to parse LSP response")
    }

    /// Sends a notification (no response expected).
    async fn notify<P: serde::Serialize>(&self, method: &str, params: P) -> Result<()> {
        let notification = NotificationMessage {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params: serde_json::to_value(params)?,
        };

        self.send_message(&notification).await
    }

    /// Sends a JSON-RPC message with Content-Length header.
    async fn send_message<T: serde::Serialize>(&self, message: &T) -> Result<()> {
        let body = serde_json::to_string(message)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());

        trace!("Sending LSP message: {}", body);

        let mut stdin = self.stdin.lock().await;
        stdin.write_all(header.as_bytes()).await?;
        stdin.write_all(body.as_bytes()).await?;
        stdin.flush().await?;

        Ok(())
    }

    /// Performs the LSP initialize handshake.
    pub async fn initialize(&mut self, root: &Path) -> Result<InitializeResult> {
        let root_uri: Uri = format!("file://{}", root.display())
            .parse()
            .map_err(|e| anyhow!("Invalid root path {:?}: {}", root, e))?;

        let params = InitializeParams {
            process_id: Some(std::process::id()),
            capabilities: ClientCapabilities {
                general: Some(lsp_types::GeneralClientCapabilities {
                    position_encodings: Some(vec![
                        PositionEncodingKind::UTF8,
                        PositionEncodingKind::UTF16,
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root_uri,
                name: root
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "workspace".to_string()),
            }]),
            ..Default::default()
        };

        let result: InitializeResult = self.request("initialize", params).await?;

        // Extract negotiated encoding
        if let Some(capabilities) = &result.capabilities.position_encoding {
            self.encoding = capabilities.clone();
            debug!("Negotiated position encoding: {:?}", self.encoding);
        } else {
            debug!("Server did not specify position encoding, defaulting to UTF-16");
            self.encoding = PositionEncodingKind::UTF16;
        }

        // Send initialized notification
        self.notify("initialized", InitializedParams {}).await?;

        Ok(result)
    }

    /// Returns the negotiated position encoding.
    pub fn encoding(&self) -> PositionEncodingKind {
        self.encoding.clone()
    }

    /// Sends shutdown request and exit notification.
    pub async fn shutdown(&mut self) -> Result<()> {
        // shutdown response varies by server (null, true, etc.) - ignore result
        let _: serde_json::Value = self.request("shutdown", serde_json::Value::Null).await?;
        self.notify("exit", serde_json::Value::Null).await?;
        Ok(())
    }

    /// Notifies the LSP server that a document was opened.
    pub async fn did_open(&self, params: DidOpenTextDocumentParams) -> Result<()> {
        self.notify("textDocument/didOpen", params).await
    }

    /// Notifies the LSP server that a document changed.
    pub async fn did_change(&self, params: DidChangeTextDocumentParams) -> Result<()> {
        self.notify("textDocument/didChange", params).await
    }

    /// Notifies the LSP server that a document was closed.
    pub async fn did_close(&self, params: DidCloseTextDocumentParams) -> Result<()> {
        self.notify("textDocument/didClose", params).await
    }

    /// Gets hover information for a position in a document.
    pub async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        self.request("textDocument/hover", params).await
    }

    /// Gets the definition location for a symbol.
    pub async fn definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.request("textDocument/definition", params).await
    }

    /// Gets the type definition location for a symbol.
    pub async fn type_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.request("textDocument/typeDefinition", params).await
    }

    /// Gets implementation locations for a symbol.
    pub async fn implementation(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.request("textDocument/implementation", params).await
    }

    /// Gets all references to a symbol.
    pub async fn references(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<lsp_types::Location>>> {
        self.request("textDocument/references", params).await
    }

    /// Gets document symbols (outline) for a file.
    pub async fn document_symbols(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        self.request("textDocument/documentSymbol", params).await
    }

    /// Searches for symbols across the workspace.
    pub async fn workspace_symbols(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<WorkspaceSymbolResponse>> {
        self.request("workspace/symbol", params).await
    }

    /// Gets code actions (quick fixes, refactorings) for a range.
    pub async fn code_actions(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>> {
        self.request("textDocument/codeAction", params).await
    }

    /// Computes a rename operation across the workspace.
    pub async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        self.request("textDocument/rename", params).await
    }

    /// Gets completion suggestions at a position.
    pub async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        self.request("textDocument/completion", params).await
    }

    /// Gets signature help for a function call.
    pub async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
        self.request("textDocument/signatureHelp", params).await
    }

    /// Formats an entire document.
    pub async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        self.request("textDocument/formatting", params).await
    }

    /// Formats a range within a document.
    pub async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        self.request("textDocument/rangeFormatting", params).await
    }

    /// Prepares call hierarchy for a position.
    pub async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        self.request("textDocument/prepareCallHierarchy", params)
            .await
    }

    /// Gets incoming calls to a call hierarchy item.
    pub async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        self.request("callHierarchy/incomingCalls", params).await
    }

    /// Gets outgoing calls from a call hierarchy item.
    pub async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        self.request("callHierarchy/outgoingCalls", params).await
    }

    /// Prepares type hierarchy for a position.
    pub async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        self.request("textDocument/prepareTypeHierarchy", params)
            .await
    }

    /// Gets supertypes of a type hierarchy item.
    pub async fn supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        self.request("typeHierarchy/supertypes", params).await
    }

    /// Gets subtypes of a type hierarchy item.
    pub async fn subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        self.request("typeHierarchy/subtypes", params).await
    }

    /// Gets cached diagnostics for a specific URI.
    pub async fn get_diagnostics(&self, uri: &Uri) -> Vec<Diagnostic> {
        let cache = self.diagnostics.lock().await;
        cache.get(uri).cloned().unwrap_or_default()
    }

    /// Returns true if the LSP server connection is still alive.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

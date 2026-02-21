// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

use anyhow::{Context, Result, anyhow};
use bytes::BytesMut;
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    ClientCapabilities, CodeActionParams, CodeActionResponse, CompletionParams, CompletionResponse,
    Diagnostic, DidChangeTextDocumentParams, DidChangeWorkspaceFoldersParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    DocumentFormattingParams, DocumentRangeFormattingParams, DocumentSymbolParams,
    DocumentSymbolResponse, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams,
    InitializeParams, InitializeResult, InitializedParams, PositionEncodingKind, ProgressParams,
    PublishDiagnosticsParams, ReferenceParams, RenameParams, SignatureHelp, SignatureHelpParams,
    TextDocumentIdentifier, TextEdit, TypeHierarchyItem, TypeHierarchyPrepareParams,
    TypeHierarchySubtypesParams, TypeHierarchySupertypesParams, Uri, WorkspaceEdit,
    WorkspaceFolder, WorkspaceFoldersChangeEvent, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, Notify, oneshot};
use tracing::{debug, error, trace, warn};

use super::protocol::{self, NotificationMessage, RequestId, RequestMessage, ResponseMessage};
use super::state::{ProgressTracker, ServerState, ServerStatus};
use crate::session::{EventBroadcaster, EventKind};

/// Cached diagnostics for a file.
pub type DiagnosticsCache = Arc<Mutex<HashMap<Uri, Vec<Diagnostic>>>>;

/// Result of waiting for diagnostics to update after a file change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiagnosticsWaitResult {
    /// Diagnostics generation advanced past the snapshot and activity settled.
    Updated,
    /// Server went completely silent (no notifications, no active progress
    /// tokens) for the inactivity duration while still alive. The caller
    /// should re-send `didSave` to nudge the server and retry.
    Inactive,
    /// Server process died during the wait.
    ServerDied,
}

/// Default timeout for LSP requests.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Time after spawn during which we consider the server to be "warming up".
pub const WARMUP_PERIOD: Duration = Duration::from_secs(10);

/// Timeout for waiting for fresh diagnostics after a file change.
/// Used as the inactivity threshold (silence with no notifications or
/// active progress tokens) and as the Phase 2 settle timeout.
pub(crate) const DIAGNOSTICS_TIMEOUT: Duration = Duration::from_secs(30);

/// Manages communication with an LSP server process.
pub struct LspClient {
    next_id: AtomicI64,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<ResponseMessage>>>>,
    diagnostics: DiagnosticsCache,
    /// Per-URI generation counter, incremented on each `publishDiagnostics`.
    diagnostics_generation: Arc<Mutex<HashMap<Uri, u64>>>,
    /// Wakes waiters when any URI receives fresh diagnostics.
    diagnostics_notify: Arc<Notify>,
    /// Whether this server has ever published diagnostics.
    has_published_diagnostics: Arc<AtomicBool>,
    /// Incremented on every incoming server notification; used by the
    /// activity-settle loop to detect when the server goes quiet.
    activity_counter: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
    encoding: PositionEncodingKind,
    /// Progress tracking for `$/progress` notifications.
    progress: Arc<Mutex<ProgressTracker>>,
    /// Time when this client was spawned.
    spawn_time: Instant,
    /// Current server state (0=Initializing, 1=Indexing, 2=Ready, 3=Dead).
    state: Arc<AtomicU8>,
    /// The language identifier (e.g., "rust", "python") for error attribution.
    language: String,
    /// Whether the server supports dynamic workspace folder changes
    /// (both `supported` and `change_notifications` are advertised).
    supports_workspace_folders: bool,
    _reader_handle: tokio::task::JoinHandle<()>,
    child: Child,
}

impl LspClient {
    /// Spawns the LSP server process and starts the response reader task.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The server process cannot be spawned.
    /// - Stdin or stdout cannot be captured.
    pub fn spawn(
        program: &str,
        args: &[&str],
        language: &str,
        broadcaster: EventBroadcaster,
    ) -> Result<Self> {
        Self::spawn_inner(program, args, language, broadcaster, Stdio::inherit())
    }

    /// Spawns the LSP server with stderr suppressed (for `catenary doctor`).
    ///
    /// # Errors
    ///
    /// Returns an error if the server process cannot be spawned.
    pub fn spawn_quiet(
        program: &str,
        args: &[&str],
        language: &str,
        broadcaster: EventBroadcaster,
    ) -> Result<Self> {
        Self::spawn_inner(program, args, language, broadcaster, Stdio::null())
    }

    fn spawn_inner(
        program: &str,
        args: &[&str],
        language: &str,
        broadcaster: EventBroadcaster,
        stderr: Stdio,
    ) -> Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .with_context(|| format!("Failed to spawn LSP server: {program}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("stdin not captured"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("stdout not captured"))?;

        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<ResponseMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let diagnostics: DiagnosticsCache = Arc::new(Mutex::new(HashMap::new()));
        let diagnostics_generation: Arc<Mutex<HashMap<Uri, u64>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let diagnostics_notify = Arc::new(Notify::new());
        let has_published_diagnostics = Arc::new(AtomicBool::new(false));
        let activity_counter = Arc::new(AtomicU64::new(0));
        let alive = Arc::new(AtomicBool::new(true));
        let progress = Arc::new(Mutex::new(ProgressTracker::new()));
        let state = Arc::new(AtomicU8::new(ServerState::Initializing.as_u8()));

        // Broadcast initial state
        broadcaster.send(EventKind::ServerState {
            language: language.to_string(),
            state: "Initializing".to_string(),
        });

        let reader_handle = tokio::spawn(Self::reader_task(
            stdin.clone(),
            stdout,
            pending.clone(),
            diagnostics.clone(),
            diagnostics_generation.clone(),
            diagnostics_notify.clone(),
            has_published_diagnostics.clone(),
            activity_counter.clone(),
            alive.clone(),
            progress.clone(),
            state.clone(),
            language.to_string(),
            broadcaster,
        ));

        Ok(Self {
            next_id: AtomicI64::new(1),
            stdin,
            pending,
            diagnostics,
            diagnostics_generation,
            diagnostics_notify,
            has_published_diagnostics,
            activity_counter,
            alive,
            encoding: PositionEncodingKind::UTF16, // Default per spec
            progress,
            spawn_time: Instant::now(),
            state,
            language: language.to_string(),
            supports_workspace_folders: false,
            _reader_handle: reader_handle,
            child,
        })
    }

    /// Background task that reads LSP messages and routes responses to pending requests.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "Internal task requires many handles to manage client state"
    )]
    async fn reader_task(
        stdin: Arc<Mutex<ChildStdin>>,
        stdout: ChildStdout,
        pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<ResponseMessage>>>>,
        diagnostics: DiagnosticsCache,
        diagnostics_generation: Arc<Mutex<HashMap<Uri, u64>>>,
        diagnostics_notify: Arc<Notify>,
        has_published_diagnostics: Arc<AtomicBool>,
        activity_counter: Arc<AtomicU64>,
        alive: Arc<AtomicBool>,
        progress: Arc<Mutex<ProgressTracker>>,
        state: Arc<AtomicU8>,
        language: String,
        broadcaster: EventBroadcaster,
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
                        // Server Request
                        debug!("Received server request: {} (id: {})", method, id);

                        let request_id =
                            serde_json::from_value(id.clone()).unwrap_or(RequestId::Number(0));

                        let response = match method {
                            "workspace/configuration" => {
                                Self::handle_configuration_request(&value, request_id)
                            }
                            _ => {
                                // MethodNotFound for unsupported requests
                                ResponseMessage {
                                    jsonrpc: "2.0".to_string(),
                                    id: Some(request_id),
                                    result: None,
                                    error: Some(protocol::ResponseError {
                                        code: -32601,
                                        message: format!(
                                            "Method '{method}' not supported by client"
                                        ),
                                        data: None,
                                    }),
                                }
                            }
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
                            Self::handle_notification(
                                &notification,
                                &diagnostics,
                                &diagnostics_generation,
                                &diagnostics_notify,
                                &has_published_diagnostics,
                                &progress,
                                &state,
                                &language,
                                &broadcaster,
                            )
                            .await;
                            activity_counter.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                } else if value.get("id").is_some() {
                    // Response
                    if let Ok(response) = serde_json::from_value::<ResponseMessage>(value)
                        && let Some(id) = &response.id
                    {
                        let mut pending = pending.lock().await;
                        if let Some(sender) = pending.remove(id) {
                            let _ = sender.send(response);
                        } else {
                            warn!("Received response for unknown request id: {:?}", id);
                        }
                    }
                } else {
                    warn!("Unknown message format: {}", message_str);
                }
            }
        }

        // Mark server as dead
        alive.store(false, Ordering::SeqCst);
        state.store(ServerState::Dead.as_u8(), Ordering::SeqCst);
        warn!("LSP reader task exiting - server connection lost");
    }

    /// Handles `workspace/configuration` requests from the server.
    ///
    /// Returns an empty object for each requested configuration item,
    /// allowing the server to fall back to its built-in defaults.
    fn handle_configuration_request(value: &serde_json::Value, id: RequestId) -> ResponseMessage {
        let item_count = value
            .get("params")
            .and_then(|p| p.get("items"))
            .and_then(|i| i.as_array())
            .map_or(1, Vec::len);

        let results: Vec<serde_json::Value> = (0..item_count)
            .map(|_| serde_json::Value::Object(serde_json::Map::new()))
            .collect();

        ResponseMessage {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            result: Some(serde_json::Value::Array(results)),
            error: None,
        }
    }

    /// Handles incoming LSP notifications.
    #[allow(
        clippy::too_many_arguments,
        reason = "Internal notification handler requires many shared-state handles"
    )]
    async fn handle_notification(
        notification: &NotificationMessage,
        diagnostics: &DiagnosticsCache,
        diagnostics_generation: &Arc<Mutex<HashMap<Uri, u64>>>,
        diagnostics_notify: &Notify,
        has_published_diagnostics: &Arc<AtomicBool>,
        progress: &Arc<Mutex<ProgressTracker>>,
        state: &Arc<AtomicU8>,
        language: &str,
        broadcaster: &EventBroadcaster,
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
                    has_published_diagnostics.store(true, Ordering::SeqCst);

                    let mut cache = diagnostics.lock().await;
                    cache.insert(params.uri.clone(), params.diagnostics);
                    drop(cache);

                    // Bump generation counter and wake waiters
                    let mut generations = diagnostics_generation.lock().await;
                    let counter = generations.entry(params.uri).or_insert(0);
                    *counter += 1;
                    drop(generations);
                    diagnostics_notify.notify_waiters();
                } else {
                    warn!("Failed to parse publishDiagnostics params");
                }
            }
            "$/progress" => {
                if let Ok(params) =
                    serde_json::from_value::<ProgressParams>(notification.params.clone())
                {
                    let mut tracker = progress.lock().await;
                    tracker.update(&params);

                    // Update state based on progress
                    let current_state = ServerState::from_u8(state.load(Ordering::SeqCst));
                    if current_state != ServerState::Dead {
                        if tracker.is_busy() {
                            state.store(ServerState::Indexing.as_u8(), Ordering::SeqCst);
                            if let Some(p) = tracker.primary_progress() {
                                debug!("Progress: {} {}%", p.title, p.percentage.unwrap_or(0));
                                // Broadcast progress event
                                broadcaster.send(EventKind::Progress {
                                    language: language.to_string(),
                                    title: p.title.clone(),
                                    message: p.message.clone(),
                                    percentage: p.percentage,
                                });
                            }
                        } else {
                            state.store(ServerState::Ready.as_u8(), Ordering::SeqCst);
                            debug!("Server ready (progress completed)");
                            // Broadcast ready event
                            broadcaster.send(EventKind::ProgressEnd {
                                language: language.to_string(),
                            });
                        }
                    }
                } else {
                    warn!("Failed to parse $/progress params");
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
        let params_value = serde_json::to_value(params)?;

        // Retry loop for ContentModified errors
        for i in 0..3 {
            let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::SeqCst));

            let request = RequestMessage {
                jsonrpc: "2.0".to_string(),
                id: id.clone(),
                method: method.to_string(),
                params: params_value.clone(),
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
                Ok(Err(_)) => return Err(anyhow!("[{}] server closed connection", self.language)),
                Err(_) => {
                    self.pending.lock().await.remove(&id);
                    return Err(anyhow!("[{}] request '{method}' timed out", self.language));
                }
            };

            if let Some(error) = response.error {
                // Check for ContentModified (-32801) or RequestCancelled (-32800)
                if error.code == -32801 || error.code == -32800 {
                    debug!(
                        "LSP request '{}' cancelled/modified, retrying ({}/3)...",
                        method,
                        i + 1
                    );
                    tokio::time::sleep(Duration::from_millis(
                        100 * u64::try_from(i + 1).unwrap_or(1),
                    ))
                    .await;
                    continue;
                }
                return Err(anyhow!(
                    "[{}] LSP error {}: {}",
                    self.language,
                    error.code,
                    error.message
                ));
            }

            let result = response.result.unwrap_or(serde_json::Value::Null);
            return serde_json::from_value(result)
                .with_context(|| format!("[{}] failed to parse LSP response", self.language));
        }

        Err(anyhow!(
            "[{}] request '{method}' failed after retries",
            self.language
        ))
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
    async fn send_message<T: serde::Serialize + Sync>(&self, message: &T) -> Result<()> {
        let body = serde_json::to_string(message)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());

        trace!("Sending LSP message: {}", body);

        let mut stdin = self.stdin.lock().await;
        stdin.write_all(header.as_bytes()).await?;
        stdin.write_all(body.as_bytes()).await?;
        stdin.flush().await?;
        drop(stdin);

        Ok(())
    }

    /// Performs the LSP initialize handshake.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A root path is invalid.
    /// - The initialize request fails.
    /// - The server fails to respond.
    pub async fn initialize(
        &mut self,
        roots: &[PathBuf],
        initialization_options: Option<serde_json::Value>,
    ) -> Result<InitializeResult> {
        let workspace_folders: Vec<WorkspaceFolder> = roots
            .iter()
            .map(|root| {
                let uri: Uri = format!("file://{}", root.display())
                    .parse()
                    .map_err(|e| anyhow!("Invalid root path {}: {e}", root.display()))?;
                Ok(WorkspaceFolder {
                    uri,
                    name: root.file_name().map_or_else(
                        || "workspace".to_string(),
                        |s| s.to_string_lossy().to_string(),
                    ),
                })
            })
            .collect::<Result<Vec<_>>>()?;

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
                text_document: Some(lsp_types::TextDocumentClientCapabilities {
                    code_action: Some(lsp_types::CodeActionClientCapabilities {
                        code_action_literal_support: Some(lsp_types::CodeActionLiteralSupport {
                            code_action_kind: lsp_types::CodeActionKindLiteralSupport {
                                value_set: vec![
                                    "quickfix".to_string(),
                                    "refactor".to_string(),
                                    "refactor.extract".to_string(),
                                    "refactor.inline".to_string(),
                                    "refactor.rewrite".to_string(),
                                    "source".to_string(),
                                    "source.organizeImports".to_string(),
                                ],
                            },
                        }),
                        data_support: Some(true),
                        resolve_support: Some(lsp_types::CodeActionCapabilityResolveSupport {
                            properties: vec!["edit".to_string()],
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                workspace: Some(lsp_types::WorkspaceClientCapabilities {
                    workspace_folders: Some(true),
                    configuration: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            },
            workspace_folders: Some(workspace_folders),
            initialization_options,
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

        // Extract workspace folders capability
        let wf = result
            .capabilities
            .workspace
            .as_ref()
            .and_then(|ws| ws.workspace_folders.as_ref());

        let supported = wf.and_then(|wf| wf.supported).unwrap_or(false);
        let accepts_changes = wf
            .and_then(|wf| wf.change_notifications.as_ref())
            .is_some_and(|cn| {
                matches!(
                    cn,
                    lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_)
                )
            });

        self.supports_workspace_folders = supported && accepts_changes;
        debug!(
            "Server workspace folders support: {} (supported={}, change_notifications={})",
            self.supports_workspace_folders, supported, accepts_changes
        );

        // Send initialized notification
        self.notify("initialized", InitializedParams {}).await?;

        // Mark as ready (server may later report progress if indexing)
        self.state
            .store(ServerState::Ready.as_u8(), Ordering::SeqCst);

        Ok(result)
    }

    /// Returns the negotiated position encoding.
    pub fn encoding(&self) -> PositionEncodingKind {
        self.encoding.clone()
    }

    /// Sends shutdown request and exit notification.
    ///
    /// # Errors
    ///
    /// Returns an error if the shutdown request or exit notification fails.
    pub async fn shutdown(&mut self) -> Result<()> {
        // shutdown response varies by server (null, true, etc.) - ignore result
        let _: serde_json::Value = self.request("shutdown", serde_json::Value::Null).await?;
        self.notify("exit", serde_json::Value::Null).await?;
        Ok(())
    }

    /// Notifies the LSP server that a document was opened.
    ///
    /// # Errors
    ///
    /// Returns an error if the notification fails.
    pub async fn did_open(&self, params: DidOpenTextDocumentParams) -> Result<()> {
        self.notify("textDocument/didOpen", params).await
    }

    /// Notifies the LSP server that a document changed.
    ///
    /// # Errors
    ///
    /// Returns an error if the notification fails.
    pub async fn did_change(&self, params: DidChangeTextDocumentParams) -> Result<()> {
        self.notify("textDocument/didChange", params).await
    }

    /// Notifies the LSP server that a document was saved.
    ///
    /// This triggers flycheck (e.g., `cargo check`) on servers that only
    /// run diagnostics on save, like rust-analyzer.
    ///
    /// # Errors
    ///
    /// Returns an error if the notification fails.
    pub async fn did_save(&self, uri: Uri) -> Result<()> {
        self.notify(
            "textDocument/didSave",
            DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier { uri },
                text: None,
            },
        )
        .await
    }

    /// Notifies the LSP server that a document was closed.
    ///
    /// # Errors
    ///
    /// Returns an error if the notification fails.
    pub async fn did_close(&self, params: DidCloseTextDocumentParams) -> Result<()> {
        self.notify("textDocument/didClose", params).await
    }

    /// Notifies the LSP server that workspace folders changed.
    ///
    /// # Errors
    ///
    /// Returns an error if the notification fails.
    pub async fn did_change_workspace_folders(
        &self,
        added: Vec<WorkspaceFolder>,
        removed: Vec<WorkspaceFolder>,
    ) -> Result<()> {
        self.notify(
            "workspace/didChangeWorkspaceFolders",
            DidChangeWorkspaceFoldersParams {
                event: WorkspaceFoldersChangeEvent { added, removed },
            },
        )
        .await
    }

    /// Gets hover information for a position in a document.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        self.request("textDocument/hover", params).await
    }

    /// Gets the definition location for a symbol.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.request("textDocument/definition", params).await
    }

    /// Gets the type definition location for a symbol.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn type_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.request("textDocument/typeDefinition", params).await
    }

    /// Gets implementation locations for a symbol.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn implementation(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.request("textDocument/implementation", params).await
    }

    /// Gets all references to a symbol.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn references(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<lsp_types::Location>>> {
        self.request("textDocument/references", params).await
    }

    /// Gets document symbols (outline) for a file.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn document_symbols(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        self.request("textDocument/documentSymbol", params).await
    }

    /// Searches for symbols across the workspace.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn workspace_symbols(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<WorkspaceSymbolResponse>> {
        self.request("workspace/symbol", params).await
    }

    /// Gets code actions (quick fixes, refactorings) for a range.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn code_actions(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>> {
        self.request("textDocument/codeAction", params).await
    }

    /// Resolves a code action (e.g. fills in the 'edit' property).
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn resolve_code_action(
        &self,
        code_action: lsp_types::CodeAction,
    ) -> Result<lsp_types::CodeAction> {
        self.request("codeAction/resolve", code_action).await
    }

    /// Computes a rename operation across the workspace.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        self.request("textDocument/rename", params).await
    }

    /// Gets completion suggestions at a position.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        self.request("textDocument/completion", params).await
    }

    /// Gets signature help for a function call.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
        self.request("textDocument/signatureHelp", params).await
    }

    /// Formats an entire document.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        self.request("textDocument/formatting", params).await
    }

    /// Formats a range within a document.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        self.request("textDocument/rangeFormatting", params).await
    }

    /// Prepares call hierarchy for a position.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        self.request("textDocument/prepareCallHierarchy", params)
            .await
    }

    /// Gets incoming calls to a call hierarchy item.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        self.request("callHierarchy/incomingCalls", params).await
    }

    /// Gets outgoing calls from a call hierarchy item.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        self.request("callHierarchy/outgoingCalls", params).await
    }

    /// Prepares type hierarchy for a position.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        self.request("textDocument/prepareTypeHierarchy", params)
            .await
    }

    /// Gets supertypes of a type hierarchy item.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        self.request("typeHierarchy/supertypes", params).await
    }

    /// Gets subtypes of a type hierarchy item.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
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

    /// Returns the current diagnostics generation for a URI.
    ///
    /// Callers should snapshot this *before* sending a change notification,
    /// then pass the snapshot to [`wait_for_diagnostics_update`] to ensure
    /// the returned diagnostics reflect that specific change.
    pub async fn diagnostics_generation(&self, uri: &Uri) -> u64 {
        let generations = self.diagnostics_generation.lock().await;
        generations.get(uri).copied().unwrap_or(0)
    }

    /// Waits until the server is both quiet and not busy with background work.
    ///
    /// Polls every 100 ms, tracking when the last activity occurred (either a
    /// notification via `activity_counter` or an active progress token). Only
    /// returns once the server has been quiet for `settle_duration` **and**
    /// has no active progress tokens (e.g., flycheck).
    ///
    /// Returns `true` if settled or the overall timeout expired, `false` if
    /// the server died.
    async fn wait_for_activity_settle(&self, settle_duration: Duration, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        let poll_interval = Duration::from_millis(100);
        let mut last_counter = self.activity_counter.load(Ordering::SeqCst);
        let mut last_activity = tokio::time::Instant::now();

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return true;
            }

            tokio::time::sleep(remaining.min(poll_interval)).await;

            let current_counter = self.activity_counter.load(Ordering::SeqCst);
            let has_active_progress = self.progress.lock().await.is_busy();

            if current_counter != last_counter {
                last_counter = current_counter;
                last_activity = tokio::time::Instant::now();
            }

            if has_active_progress {
                last_activity = tokio::time::Instant::now();
            }

            let quiet = last_activity.elapsed();
            if quiet >= settle_duration && !has_active_progress {
                return true;
            }

            if !self.is_alive() {
                return false;
            }
        }
    }

    /// Waits until diagnostics for the URI advance past `snapshot`, then waits
    /// for the server's notification stream to go quiet.
    ///
    /// `snapshot` should be obtained via [`diagnostics_generation`] **before**
    /// sending the change that should trigger new diagnostics. This ensures
    /// no race window between sending the change and starting the wait.
    ///
    /// Phase 1 waits for diagnostics generation to advance. It tracks server
    /// activity (notification counter and progress tokens) and keeps waiting
    /// as long as the server shows signs of life. If the server goes
    /// completely silent for `timeout` with no activity, returns
    /// [`DiagnosticsWaitResult::Inactive`] so the caller can re-send
    /// `didSave` to nudge the server and retry.
    ///
    /// After the first diagnostics update, the settle phase (phase 2) polls
    /// the notification counter and progress tracker, returning only once
    /// the server has been quiet for 2 seconds with no active background work.
    pub(crate) async fn wait_for_diagnostics_update(
        &self,
        uri: &Uri,
        snapshot: u64,
        timeout: Duration,
    ) -> DiagnosticsWaitResult {
        if !self.has_published_diagnostics.load(Ordering::SeqCst) {
            // Server has never published diagnostics. During warmup we
            // give it the remaining warmup time; past warmup we give a
            // short grace period. Servers like json-languageserver only
            // publish after the first didOpen, so they miss the warmup
            // window entirely — the grace period covers that case.
            let grace = if self.is_warming_up() {
                WARMUP_PERIOD.saturating_sub(self.spawn_time.elapsed())
            } else {
                Duration::from_secs(5)
            };
            if grace.is_zero() {
                return DiagnosticsWaitResult::Updated;
            }
            let deadline = tokio::time::Instant::now() + grace;
            loop {
                if self.diagnostics_generation(uri).await > snapshot {
                    // Server published diagnostics — fall through to the
                    // normal settle logic below.
                    break;
                }
                if !self.is_alive() {
                    return DiagnosticsWaitResult::ServerDied;
                }
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return DiagnosticsWaitResult::Updated;
                }
                match tokio::time::timeout(remaining, self.diagnostics_notify.notified()).await {
                    Ok(()) => {}
                    Err(_) => return DiagnosticsWaitResult::Updated,
                }
            }
        }

        // Phase 1: Wait for diagnostics generation to advance past snapshot.
        //
        // Tracks server activity (notification counter + progress tokens) to
        // distinguish "slow but working" from "genuinely hung." As long as
        // the server shows signs of life, the wait continues. If the server
        // goes completely silent for `timeout`, returns `Inactive` so the
        // caller can nudge the server (e.g., re-send `didSave`) and retry.
        let poll_interval = Duration::from_millis(100);
        let mut last_counter = self.activity_counter.load(Ordering::SeqCst);
        let mut last_activity = tokio::time::Instant::now();

        loop {
            if self.diagnostics_generation(uri).await > snapshot {
                break;
            }

            if !self.is_alive() {
                return DiagnosticsWaitResult::ServerDied;
            }

            tokio::time::sleep(poll_interval).await;

            let current_counter = self.activity_counter.load(Ordering::SeqCst);
            let has_active_progress = self.progress.lock().await.is_busy();

            if current_counter != last_counter {
                last_counter = current_counter;
                last_activity = tokio::time::Instant::now();
            }

            if has_active_progress {
                last_activity = tokio::time::Instant::now();
            }

            // Server has been completely silent for the timeout duration.
            // Return Inactive so the caller can nudge and retry rather than
            // giving up entirely.
            if last_activity.elapsed() >= timeout && !has_active_progress {
                return DiagnosticsWaitResult::Inactive;
            }
        }

        // Phase 2: Wait for server activity to settle.
        // After the first diagnostics update, the server may still be publishing
        // additional diagnostic rounds or running flycheck (cargo check). The
        // settle polls every 100 ms and tracks the last notification or active
        // progress. It returns only after 2 seconds of silence with no active
        // progress tokens, which bridges the flycheck debounce gap and waits
        // for cargo check to finish.
        if self
            .wait_for_activity_settle(Duration::from_secs(2), timeout)
            .await
        {
            DiagnosticsWaitResult::Updated
        } else {
            DiagnosticsWaitResult::ServerDied
        }
    }

    /// Returns the language identifier for this client (e.g., "rust", "python").
    pub fn language(&self) -> &str {
        &self.language
    }

    /// Returns whether the server supports dynamic workspace folder changes.
    pub const fn supports_workspace_folders(&self) -> bool {
        self.supports_workspace_folders
    }

    /// Returns whether the LSP server process is still running.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Returns the current server state.
    pub fn server_state(&self) -> ServerState {
        ServerState::from_u8(self.state.load(Ordering::SeqCst))
    }

    /// Returns time since server spawned.
    pub fn uptime(&self) -> Duration {
        self.spawn_time.elapsed()
    }

    /// Returns true if server is in warmup period (recently spawned).
    pub fn is_warming_up(&self) -> bool {
        self.spawn_time.elapsed() < WARMUP_PERIOD
    }

    /// Returns true if server is ready to handle requests.
    pub fn is_ready(&self) -> bool {
        let state = self.server_state();
        if state != ServerState::Ready || !self.is_alive() {
            return false;
        }

        // Even if state is Ready, if we just spawned, wait a bit to see if
        // the server starts indexing (e.g. rust-analyzer takes a moment to send $/progress).
        if self.spawn_time.elapsed() < Duration::from_millis(3000) {
            return false;
        }

        true
    }

    /// Returns detailed status for this server.
    pub async fn status(&self, language: String) -> ServerStatus {
        let (title, message, percentage) = {
            let progress = self.progress.lock().await;
            let primary = progress.primary_progress();
            let title = primary.map(|p| p.title.clone());
            let message = primary.and_then(|p| p.message.clone());
            let percentage = primary.and_then(|p| p.percentage);
            drop(progress);
            (title, message, percentage)
        };

        ServerStatus {
            language,
            state: self.server_state(),
            progress_title: title,
            progress_message: message,
            progress_percentage: percentage,
            uptime_secs: self.uptime().as_secs(),
        }
    }

    /// Waits until server is ready (not indexing).
    ///
    /// Returns `true` if ready, `false` if server died.
    pub async fn wait_ready(&self) -> bool {
        let poll_interval = Duration::from_millis(100);

        loop {
            if self.is_ready() {
                return true;
            }
            if !self.is_alive() {
                return false;
            }
            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Robustly waits for analysis to complete after a change.
    ///
    /// Includes a grace period to allow the server to start indexing.
    pub async fn wait_for_analysis(&self) -> bool {
        // Give server a moment to start indexing
        tokio::time::sleep(Duration::from_millis(1000)).await;
        self.wait_ready().await
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // We can't await a graceful LSP shutdown here because drop is sync.
        // But we MUST ensure the child process doesn't become a zombie.
        let _ = self.child.start_kill();
    }
}

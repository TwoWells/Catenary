// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

use anyhow::{Context, Result, anyhow};
use bytes::BytesMut;
use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    ClientCapabilities, CodeActionParams, CodeActionResponse, Diagnostic,
    DidChangeTextDocumentParams, DidChangeWorkspaceFoldersParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentSymbolParams,
    DocumentSymbolResponse, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams,
    InitializeParams, InitializeResult, InitializedParams, PositionEncodingKind,
    PrepareRenameResponse, ProgressParams, PublishDiagnosticsParams, ReferenceParams,
    ServerCapabilities, TextDocumentIdentifier, TextDocumentPositionParams, TypeHierarchyItem,
    TypeHierarchyPrepareParams, TypeHierarchySubtypesParams, TypeHierarchySupertypesParams, Uri,
    WorkspaceFolder, WorkspaceFoldersChangeEvent, WorkspaceSymbol, WorkspaceSymbolParams,
    WorkspaceSymbolResponse,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, Notify, oneshot};
use tracing::{debug, error, info, trace, warn};

use super::protocol::{self, NotificationMessage, RequestId, RequestMessage, ResponseMessage};
use super::state::{ProgressTracker, ServerState, ServerStatus};
use super::wait::load_aware_grace;
use crate::session::{EventBroadcaster, EventKind};

/// Cached diagnostics for a file: `(version, diagnostics)`.
///
/// `version` is the document version from `publishDiagnostics`, if the
/// server includes it. Used by [`DiagnosticsStrategy::Version`] to
/// match diagnostics to a specific document change.
pub type DiagnosticsCache = Arc<Mutex<HashMap<Uri, (Option<i32>, Vec<Diagnostic>)>>>;

/// Result of waiting for diagnostics to update after a file change.
///
/// The agent never sees infrastructure details — only "trusted
/// diagnostics are in the cache" or "nothing available."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticsWaitResult {
    /// Trusted diagnostics are in the cache — safe to read.
    Diagnostics,
    /// No trusted diagnostics available. Covers server death, budget
    /// exhaustion, and servers without version/progress support.
    Nothing,
}

/// Time after spawn during which we consider the server to be "warming up".
pub const WARMUP_PERIOD: Duration = Duration::from_secs(10);

/// CPU tick threshold for diagnostics wait: 1000 ticks = 10 CPU-seconds.
const DIAGNOSTICS_THRESHOLD: u64 = 1000;

/// CPU tick threshold for preamble windows (grace, discovery, progress grace).
const PREAMBLE_THRESHOLD: u64 = 500;

/// CPU tick threshold for request timeout: 1000 ticks = 10 CPU-seconds.
///
/// The initialize request is the most expensive, taking several seconds of
/// CPU time on complex servers. Normal requests (definition, references)
/// are sub-second, so 10 CPU-seconds is generous. The threshold only
/// counts unexplained work — if the server is sleeping (waiting on cargo,
/// NFS, etc.), it doesn't drain.
const REQUEST_THRESHOLD: u64 = 1000;

/// CPU tick threshold for `wait_ready`: 1000 ticks = 10 CPU-seconds.
const READY_THRESHOLD: u64 = 1000;

/// Poll interval for diagnostics wait main loops.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Wall-clock safety cap (5 minutes) for diagnostics wait.
const SAFETY_CAP: Duration = Duration::from_secs(300);

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
    /// Wakes waiters when capability flags (`publishes_version`, `has_sent_progress`) change.
    capability_notify: Arc<Notify>,
    /// Wakes waiters on `$/progress` state transitions.
    progress_notify: Arc<Notify>,
    /// Wakes waiters on `ServerState` changes.
    state_notify: Arc<Notify>,
    /// Whether this server has ever published diagnostics.
    has_published_diagnostics: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    encoding: PositionEncodingKind,
    /// Progress tracking for `$/progress` notifications.
    progress: Arc<Mutex<ProgressTracker>>,
    /// Time when this client was spawned.
    spawn_time: Instant,
    /// Current server state (0=Initializing, 1=Busy, 2=Ready, 3=Dead).
    state: Arc<AtomicU8>,
    /// The language identifier (e.g., "rust", "python") for error attribution.
    language: String,
    /// Whether the server supports dynamic workspace folder changes
    /// (both `supported` and `change_notifications` are advertised).
    supports_workspace_folders: bool,
    /// Whether the server has ever included `version` in `publishDiagnostics`.
    publishes_version: Arc<AtomicBool>,
    /// Whether the server has ever sent `$/progress` notifications.
    has_sent_progress: Arc<AtomicBool>,
    /// Logged once when a server is detected as lacking diagnostics support.
    logged_no_diagnostics_support: AtomicBool,
    /// Last document version sent via `did_open`/`did_change` per URI.
    /// Used to detect stale diagnostics from prior document versions.
    last_sent_version: Arc<Mutex<HashMap<Uri, i32>>>,
    /// Persistent process monitor for CPU-tick failure detection.
    monitor: std::sync::Mutex<Option<catenary_proc::ProcessMonitor>>,
    /// Whether the server advertised `textDocumentSync.save` support.
    wants_did_save: bool,
    /// Whether the server advertised `typeHierarchyProvider`.
    /// Tracked separately because `lsp_types` 0.97 omits this field from
    /// `ServerCapabilities` despite it being in the LSP 3.17 spec.
    supports_type_hierarchy: bool,
    /// The command used to spawn this server (e.g., "rust-analyzer").
    server_command: String,
    /// Server version from the `initialize` response (`ServerInfo.version`).
    /// Populated after `initialize()` completes; `None` if the server
    /// did not report a version.
    server_version: Option<String>,
    /// Server capabilities from the `initialize` response.
    /// Populated after `initialize()` completes.
    server_capabilities: ServerCapabilities,
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

    #[allow(
        clippy::too_many_lines,
        reason = "Spawn requires many sequential initialization steps"
    )]
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

        // Create ProcessMonitor from child PID (before taking stdin/stdout)
        let monitor = child.id().and_then(catenary_proc::ProcessMonitor::new);

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
        let capability_notify = Arc::new(Notify::new());
        let progress_notify = Arc::new(Notify::new());
        let state_notify = Arc::new(Notify::new());
        let has_published_diagnostics = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let progress = Arc::new(Mutex::new(ProgressTracker::new()));
        let state = Arc::new(AtomicU8::new(ServerState::Initializing.as_u8()));
        let publishes_version = Arc::new(AtomicBool::new(false));
        let has_sent_progress = Arc::new(AtomicBool::new(false));

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
            capability_notify.clone(),
            progress_notify.clone(),
            state_notify.clone(),
            has_published_diagnostics.clone(),
            alive.clone(),
            progress.clone(),
            state.clone(),
            language.to_string(),
            broadcaster,
            publishes_version.clone(),
            has_sent_progress.clone(),
        ));

        Ok(Self {
            next_id: AtomicI64::new(1),
            stdin,
            pending,
            diagnostics,
            diagnostics_generation,
            diagnostics_notify,
            capability_notify,
            progress_notify,
            state_notify,
            has_published_diagnostics,
            alive,
            encoding: PositionEncodingKind::UTF16, // Default per spec
            progress,
            spawn_time: Instant::now(),
            state,
            language: language.to_string(),
            supports_workspace_folders: false,
            publishes_version,
            has_sent_progress,
            logged_no_diagnostics_support: AtomicBool::new(false),
            last_sent_version: Arc::new(Mutex::new(HashMap::new())),
            monitor: std::sync::Mutex::new(monitor),
            wants_did_save: false,
            supports_type_hierarchy: false,
            server_command: program.to_string(),
            server_version: None,
            server_capabilities: ServerCapabilities::default(),
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
        capability_notify: Arc<Notify>,
        progress_notify: Arc<Notify>,
        state_notify: Arc<Notify>,
        has_published_diagnostics: Arc<AtomicBool>,
        alive: Arc<AtomicBool>,
        progress: Arc<Mutex<ProgressTracker>>,
        state: Arc<AtomicU8>,
        language: String,
        broadcaster: EventBroadcaster,
        publishes_version: Arc<AtomicBool>,
        has_sent_progress: Arc<AtomicBool>,
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
                            "window/workDoneProgress/create" => {
                                // Accept progress token registration so the
                                // server sends $/progress notifications.
                                ResponseMessage {
                                    jsonrpc: "2.0".to_string(),
                                    id: Some(request_id),
                                    result: Some(serde_json::Value::Null),
                                    error: None,
                                }
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
                                &capability_notify,
                                &progress_notify,
                                &state_notify,
                                &has_published_diagnostics,
                                &progress,
                                &state,
                                &language,
                                &broadcaster,
                                &publishes_version,
                                &has_sent_progress,
                            )
                            .await;
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

        // Mark server as dead and clean up orphaned progress tokens
        alive.store(false, Ordering::SeqCst);
        state.store(ServerState::Dead.as_u8(), Ordering::SeqCst);
        progress.lock().await.clear();
        diagnostics_notify.notify_waiters();
        state_notify.notify_waiters();
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
        capability_notify: &Notify,
        progress_notify: &Notify,
        state_notify: &Notify,
        has_published_diagnostics: &Arc<AtomicBool>,
        progress: &Arc<Mutex<ProgressTracker>>,
        state: &Arc<AtomicU8>,
        language: &str,
        broadcaster: &EventBroadcaster,
        publishes_version: &Arc<AtomicBool>,
        has_sent_progress: &Arc<AtomicBool>,
    ) {
        match notification.method.as_str() {
            "textDocument/publishDiagnostics" => {
                if let Ok(params) =
                    serde_json::from_value::<PublishDiagnosticsParams>(notification.params.clone())
                {
                    debug!(
                        "Received {} diagnostics for {:?} (version={:?})",
                        params.diagnostics.len(),
                        params.uri.as_str(),
                        params.version,
                    );
                    has_published_diagnostics.store(true, Ordering::SeqCst);

                    // Track whether server provides version in diagnostics
                    if params.version.is_some() && !publishes_version.swap(true, Ordering::SeqCst) {
                        capability_notify.notify_waiters();
                    }

                    let mut cache = diagnostics.lock().await;
                    cache.insert(params.uri.clone(), (params.version, params.diagnostics));
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
                    if !has_sent_progress.swap(true, Ordering::SeqCst) {
                        capability_notify.notify_waiters();
                    }

                    let mut tracker = progress.lock().await;
                    tracker.update(&params);

                    // Update state based on progress.
                    // The Dead guard is the only exclusion — Stuck servers
                    // that send progress are naturally recovered here
                    // (transitioned to Busy/Ready like any other state).
                    let current_state = ServerState::from_u8(state.load(Ordering::SeqCst));
                    if current_state != ServerState::Dead {
                        if tracker.is_busy() {
                            state.store(ServerState::Busy.as_u8(), Ordering::SeqCst);
                            if tracker.broadcast_changed()
                                && let Some(p) = tracker.primary_progress()
                            {
                                debug!("Progress: {} {}%", p.title, p.percentage.unwrap_or(0));
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
                            broadcaster.send(EventKind::ProgressEnd {
                                language: language.to_string(),
                            });
                        }
                        // Fire notifies after state update
                        progress_notify.notify_waiters();
                        state_notify.notify_waiters();
                    }
                } else {
                    warn!("Failed to parse $/progress params");
                }
            }
            "window/logMessage" | "window/showMessage" => {
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

    /// Samples the server process via the persistent `ProcessMonitor`.
    ///
    /// Returns `(delta, state)` where delta is ticks since the last sample.
    /// Returns `None` if the process is gone or monitoring is unavailable.
    fn sample_monitor(&self) -> Option<(u64, catenary_proc::ProcessState)> {
        self.monitor.lock().ok()?.as_mut()?.sample()
    }

    /// Returns whether the server has active `$/progress` tokens.
    ///
    /// Checks the actual progress tracker instead of using `ServerState`
    /// as a proxy. `ServerState::Busy` can be set proactively (e.g., after
    /// `workspace/didChangeWorkspaceFolders`) without actual `$/progress`
    /// tokens, which would prevent the failure threshold from draining.
    fn progress_active(&self) -> bool {
        self.progress
            .try_lock()
            .map_or(true, |tracker| tracker.is_busy())
    }

    /// Sends a request and waits for the response with failure detection.
    ///
    /// Uses CPU-tick failure detection instead of wall-clock timeout.
    /// Falls back to a 30-second wall-clock timeout when the process
    /// monitor is unavailable (e.g., mockls in tests).
    /// Retries on `ContentModified` by waiting for the server to become
    /// ready before retrying.
    #[allow(
        clippy::too_many_lines,
        reason = "Request retry logic with failure detection"
    )]
    async fn request<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R> {
        let params_value = serde_json::to_value(params)?;

        // Retry loop for ContentModified errors
        for _attempt in 0..3 {
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

            // Wait for response: select on rx + failure detection timer
            let response = {
                let mut rx = rx;
                let wall_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                let mut budget = i64::try_from(REQUEST_THRESHOLD).unwrap_or(1000);

                loop {
                    tokio::select! {
                        result = &mut rx => {
                            match result {
                                Ok(resp) => break Ok(resp),
                                Err(_) => break Err(anyhow!(
                                    "[{}] server closed connection", self.language
                                )),
                            }
                        }
                        () = tokio::time::sleep(POLL_INTERVAL) => {
                            // Failure detection
                            if let Some((delta, state)) = self.sample_monitor() {
                                if state == catenary_proc::ProcessState::Dead {
                                    self.pending.lock().await.remove(&id);
                                    break Err(anyhow!(
                                        "[{}] server died during '{method}'",
                                        self.language
                                    ));
                                }
                                if state == catenary_proc::ProcessState::Running
                                    && delta > 0
                                    && !self.progress_active()
                                {
                                    budget -= i64::try_from(delta)
                                        .unwrap_or(budget);
                                }
                            } else if !self.is_alive() {
                                self.pending.lock().await.remove(&id);
                                break Err(anyhow!(
                                    "[{}] server died during '{method}'",
                                    self.language
                                ));
                            }

                            if budget <= 0 {
                                self.pending.lock().await.remove(&id);
                                break Err(anyhow!(
                                    "[{}] request '{method}' failed \
                                     (server stuck)",
                                    self.language
                                ));
                            }
                            if tokio::time::Instant::now() >= wall_deadline {
                                self.pending.lock().await.remove(&id);
                                break Err(anyhow!(
                                    "[{}] request '{method}' timed out",
                                    self.language
                                ));
                            }
                        }
                    }
                }
            }?;

            if let Some(error) = response.error {
                // Check for ContentModified (-32801) or RequestCancelled (-32800)
                if error.code == -32801 || error.code == -32800 {
                    debug!(
                        "LSP request '{}' cancelled/modified, waiting for ready then retrying...",
                        method,
                    );
                    // Wait for server to finish instead of fixed sleep
                    self.wait_ready().await;
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
    #[allow(
        clippy::too_many_lines,
        reason = "Initialize handshake has many sequential steps"
    )]
    #[allow(
        deprecated,
        reason = "root_uri is deprecated in LSP but servers like lua-language-server still require it"
    )]
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
                    synchronization: Some(lsp_types::TextDocumentSyncClientCapabilities {
                        did_save: Some(true),
                        dynamic_registration: Some(false),
                        will_save: Some(false),
                        will_save_wait_until: Some(false),
                    }),
                    publish_diagnostics: Some(lsp_types::PublishDiagnosticsClientCapabilities {
                        version_support: Some(true),
                        ..Default::default()
                    }),
                    definition: Some(lsp_types::GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(true),
                    }),
                    type_definition: Some(lsp_types::GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(true),
                    }),
                    implementation: Some(lsp_types::GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(true),
                    }),
                    declaration: Some(lsp_types::GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(true),
                    }),
                    references: Some(lsp_types::DynamicRegistrationClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    document_symbol: Some(lsp_types::DocumentSymbolClientCapabilities {
                        dynamic_registration: Some(false),
                        hierarchical_document_symbol_support: Some(true),
                        ..Default::default()
                    }),
                    call_hierarchy: Some(lsp_types::CallHierarchyClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    type_hierarchy: Some(lsp_types::TypeHierarchyClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    code_action: Some(lsp_types::CodeActionClientCapabilities {
                        dynamic_registration: Some(false),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                workspace: Some(lsp_types::WorkspaceClientCapabilities {
                    symbol: Some(lsp_types::WorkspaceSymbolClientCapabilities {
                        resolve_support: Some(lsp_types::WorkspaceSymbolResolveSupportCapability {
                            properties: vec!["location.range".to_string()],
                        }),
                        ..Default::default()
                    }),
                    workspace_folders: Some(true),
                    configuration: Some(true),
                    ..Default::default()
                }),
                window: Some(lsp_types::WindowClientCapabilities {
                    work_done_progress: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            },
            root_uri: workspace_folders.first().map(|wf| wf.uri.clone()),
            workspace_folders: Some(workspace_folders),
            initialization_options,
            ..Default::default()
        };

        let raw: serde_json::Value = self.request("initialize", params).await?;

        // Extract typeHierarchyProvider before lsp_types drops it
        // (missing from ServerCapabilities in lsp_types 0.97)
        self.supports_type_hierarchy = raw
            .get("capabilities")
            .and_then(|c| c.get("typeHierarchyProvider"))
            .is_some_and(|v| !v.is_null());

        let result: InitializeResult =
            serde_json::from_value(raw).context("failed to parse InitializeResult")?;

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

        // Extract textDocumentSync.save capability.
        // When the server advertises Options with an explicit `save` field,
        // trust it. When the server uses the short-form Kind (Full/Incremental),
        // LSP spec says this is equivalent to Options with save=SaveOptions{},
        // meaning the server accepts didSave.
        self.wants_did_save = match &result.capabilities.text_document_sync {
            Some(lsp_types::TextDocumentSyncCapability::Kind(kind)) => {
                *kind != lsp_types::TextDocumentSyncKind::NONE
            }
            Some(lsp_types::TextDocumentSyncCapability::Options(opts)) => opts.save.is_some(),
            None => false,
        };
        debug!(
            "[{}] server wants didSave: {}",
            self.language, self.wants_did_save
        );

        // Store server info and capabilities
        self.server_version = result
            .server_info
            .as_ref()
            .and_then(|si| si.version.clone());
        self.server_capabilities = result.capabilities.clone();

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

    /// Returns the server capabilities from the `initialize` response.
    pub const fn capabilities(&self) -> &ServerCapabilities {
        &self.server_capabilities
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
        let uri = params.text_document.uri.clone();
        let version = params.text_document.version;
        self.last_sent_version.lock().await.insert(uri, version);
        self.notify("textDocument/didOpen", params).await
    }

    /// Notifies the LSP server that a document changed.
    ///
    /// # Errors
    ///
    /// Returns an error if the notification fails.
    pub async fn did_change(&self, params: DidChangeTextDocumentParams) -> Result<()> {
        let uri = params.text_document.uri.clone();
        let version = params.text_document.version;
        self.last_sent_version.lock().await.insert(uri, version);
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
    /// When folders are added, proactively marks the server as
    /// [`ServerState::Busy`] so that [`wait_ready`](Self::wait_ready)
    /// blocks queries until the server is ready again.
    ///
    /// # Errors
    ///
    /// Returns an error if the notification fails.
    pub async fn did_change_workspace_folders(
        &self,
        added: Vec<WorkspaceFolder>,
        removed: Vec<WorkspaceFolder>,
    ) -> Result<()> {
        if !added.is_empty() && self.server_state() == ServerState::Ready {
            self.state
                .store(ServerState::Busy.as_u8(), Ordering::SeqCst);
        }

        self.notify(
            "workspace/didChangeWorkspaceFolders",
            DidChangeWorkspaceFoldersParams {
                event: WorkspaceFoldersChangeEvent { added, removed },
            },
        )
        .await
    }

    /// Gets hover information (signature, documentation) for a position.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        self.request("textDocument/hover", params).await
    }

    /// Tests whether a position is a renameable symbol.
    ///
    /// Returns `Some(range/placeholder)` for symbols, `None` for keywords
    /// and non-symbol positions. Used as a cheap discriminator before full
    /// enrichment in the rg-bootstrap path.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        self.request("textDocument/prepareRename", params).await
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

    /// Resolves additional properties (e.g. `location.range`) for a workspace symbol.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn workspace_symbol_resolve(
        &self,
        params: WorkspaceSymbol,
    ) -> Result<Option<WorkspaceSymbol>> {
        self.request("workspaceSymbol/resolve", params).await
    }

    /// Returns whether the server advertises `workspaceSymbol/resolve` support.
    pub fn supports_workspace_symbol_resolve(&self) -> bool {
        matches!(
            &self.server_capabilities.workspace_symbol_provider,
            Some(lsp_types::OneOf::Right(opts)) if opts.resolve_provider == Some(true)
        )
    }

    /// Returns whether the server advertises `typeHierarchyProvider`.
    pub const fn supports_type_hierarchy(&self) -> bool {
        self.supports_type_hierarchy
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

    /// Gets code actions (quick fixes, refactorings) for a range.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>> {
        self.request("textDocument/codeAction", params).await
    }

    /// Gets cached diagnostics for a specific URI.
    pub async fn get_diagnostics(&self, uri: &Uri) -> Vec<Diagnostic> {
        let cache = self.diagnostics.lock().await;
        cache
            .get(uri)
            .map(|(_, diags)| diags.clone())
            .unwrap_or_default()
    }

    /// Gets the cached diagnostics version for a URI.
    ///
    /// Returns `None` if no diagnostics have been published for this URI
    /// or if the server doesn't include version in `publishDiagnostics`.
    #[allow(dead_code, reason = "Used by diagnostics strategy tests")]
    pub(crate) async fn cached_diagnostics_version(&self, uri: &Uri) -> Option<i32> {
        let cache = self.diagnostics.lock().await;
        cache.get(uri).and_then(|(version, _)| *version)
    }

    /// Returns whether cached diagnostics match the last-sent document version.
    ///
    /// Returns `true` (assume current) when the server doesn't publish version
    /// info or when no version has been tracked for this URI — we can't
    /// distinguish stale from fresh without version data.
    async fn is_diagnostics_version_current(&self, uri: &Uri) -> bool {
        if !self.publishes_version.load(Ordering::SeqCst) {
            return true;
        }
        let sent = self.last_sent_version.lock().await;
        let Some(sent_v) = sent.get(uri).copied() else {
            return true;
        };
        drop(sent);
        let cached_v = self.cached_diagnostics_version(uri).await;
        cached_v.is_some_and(|v| v >= sent_v)
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

    /// Returns the diagnostics strategy for this server, if any.
    ///
    /// Selected based on runtime observations: whether the server has
    /// included `version` in `publishDiagnostics`, or sent `$/progress`
    /// tokens. Returns `None` for servers without either signal —
    /// they do not participate in the diagnostics lifecycle.
    ///
    /// When both signals are present, prefers `TokenMonitor` because
    /// multi-round servers (rust-analyzer, clangd, gopls) publish fast
    /// native diagnostics first (matching version), then slower flycheck
    /// results under a progress token. The Active → Idle transition
    /// spans the full work.
    pub(crate) fn diagnostics_strategy(&self) -> Option<super::diagnostics::DiagnosticsStrategy> {
        use super::diagnostics::DiagnosticsStrategy;

        if self.has_sent_progress.load(Ordering::SeqCst) {
            Some(DiagnosticsStrategy::TokenMonitor)
        } else if self.publishes_version.load(Ordering::SeqCst) {
            Some(DiagnosticsStrategy::Version)
        } else {
            None
        }
    }

    /// Returns whether this server supports the diagnostics wait lifecycle.
    ///
    /// Servers must provide at least one of:
    /// - `version` field in `publishDiagnostics` (LSP 3.15+)
    /// - `$/progress` tokens
    ///
    /// Servers without either still receive `didOpen`/`didChange` for code
    /// intelligence but do not get `didSave` and are not waited on for
    /// diagnostics.
    pub fn supports_diagnostics_wait(&self) -> bool {
        if self.publishes_version.load(Ordering::SeqCst)
            || self.has_sent_progress.load(Ordering::SeqCst)
        {
            return true;
        }
        // Log once when we determine the server lacks support (after warmup)
        if !self.is_warming_up()
            && !self
                .logged_no_diagnostics_support
                .swap(true, Ordering::SeqCst)
        {
            info!(
                "[{}] server lacks version/progress support \u{2014} diagnostics disabled",
                self.language
            );
        }
        false
    }

    /// Returns whether the server advertised `textDocumentSync.save` support.
    ///
    /// When `false`, `did_save` should not be sent — the server doesn't
    /// want it and may not run diagnostics on save.
    pub const fn wants_did_save(&self) -> bool {
        self.wants_did_save
    }

    /// Returns the PID of the server process, if available.
    #[allow(dead_code, reason = "Used by diagnostics tests and session status")]
    pub(crate) fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Waits for fresh diagnostics after a file change, using the
    /// appropriate strategy for this server.
    ///
    /// `snapshot` should be obtained via [`diagnostics_generation`] **before**
    /// sending the change that triggers new diagnostics.
    ///
    /// Uses CPU tick failure detection instead of wall-clock timeouts.
    /// The failure threshold only drains when the server process is Running
    /// with advancing ticks and no active progress — starvation, sleeping,
    /// blocked I/O, and explained work are free waits.
    ///
    /// Returns [`DiagnosticsWaitResult::Diagnostics`] when trusted
    /// diagnostics are in the cache, or [`DiagnosticsWaitResult::Nothing`]
    /// when no trusted diagnostics are available.
    #[allow(
        clippy::too_many_lines,
        reason = "Strategy dispatch requires many branches"
    )]
    pub async fn wait_for_diagnostics_update(
        &self,
        uri: &Uri,
        snapshot: u64,
    ) -> DiagnosticsWaitResult {
        use super::diagnostics::{ActivityState, DiagnosticsStrategy, ProgressMonitor};

        // ── Grace period ─────────────────────────────────────────────
        // For servers that haven't published diagnostics yet, wait for
        // the first publishDiagnostics using load-aware failure detection.
        if !self.has_published_diagnostics.load(Ordering::SeqCst) {
            let grace_ok = load_aware_grace(
                &mut || self.sample_monitor(),
                PREAMBLE_THRESHOLD,
                Some(Duration::from_secs(10)),
                &self.diagnostics_notify,
                || self.progress_active(),
                || async { self.diagnostics_generation(uri).await > snapshot },
            )
            .await;

            if !grace_ok {
                return DiagnosticsWaitResult::Nothing;
            }
        }

        // ── Strategy discovery ────────────────────────────────────────
        // Allow a short window for the server to demonstrate its strategy
        // (e.g., progress tokens sent in response to didChange).
        // Uses a wall-clock timeout: the server may be sleeping (not
        // consuming CPU) while deciding what capability to expose, so
        // tick-based thresholds would wait indefinitely.
        let strategy = if let Some(s) = self.diagnostics_strategy() {
            s
        } else {
            let discovery_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            loop {
                if let Some(s) = self.diagnostics_strategy() {
                    break s;
                }
                if !self.is_alive() || tokio::time::Instant::now() >= discovery_deadline {
                    return DiagnosticsWaitResult::Nothing;
                }
                tokio::select! {
                    () = self.capability_notify.notified() => {}
                    () = tokio::time::sleep(POLL_INTERVAL) => {}
                }
            }
        };
        debug!(
            "Diagnostics strategy: {:?} (has_progress={}, publishes_version={})",
            strategy,
            self.has_sent_progress.load(Ordering::SeqCst),
            self.publishes_version.load(Ordering::SeqCst),
        );

        let wall_deadline = tokio::time::Instant::now() + SAFETY_CAP;
        let mut budget: i64 = i64::try_from(DIAGNOSTICS_THRESHOLD).unwrap_or(1000);

        // ── Main wait loops ──────────────────────────────────────────
        match strategy {
            DiagnosticsStrategy::Version => {
                // Wait for publishDiagnostics with version >= our change.
                loop {
                    if self.diagnostics_generation(uri).await > snapshot
                        && self.is_diagnostics_version_current(uri).await
                    {
                        return DiagnosticsWaitResult::Diagnostics;
                    }

                    // Event-driven wake + failure detection
                    tokio::select! {
                        () = self.diagnostics_notify.notified() => {
                            // Check condition at top of loop
                            continue;
                        }
                        () = tokio::time::sleep(POLL_INTERVAL) => {}
                    }

                    // Failure detection
                    if let Some((delta, state)) = self.sample_monitor() {
                        if state == catenary_proc::ProcessState::Dead {
                            return DiagnosticsWaitResult::Nothing;
                        }
                        if state == catenary_proc::ProcessState::Running
                            && delta > 0
                            && !self.progress_active()
                        {
                            budget -= i64::try_from(delta).unwrap_or(budget);
                        }
                    } else if !self.is_alive() {
                        return DiagnosticsWaitResult::Nothing;
                    }

                    if budget <= 0 {
                        debug!("Version: tick budget exhausted");
                        return DiagnosticsWaitResult::Nothing;
                    }
                    if tokio::time::Instant::now() >= wall_deadline {
                        debug!("Version: safety cap reached");
                        return DiagnosticsWaitResult::Nothing;
                    }
                }
            }
            DiagnosticsStrategy::TokenMonitor => {
                let mut monitor =
                    super::diagnostics::TokenMonitor::new(self.state.clone(), self.alive.clone());
                let mut ever_active = false;

                // Progress grace: if diagnostics arrive before progress tokens,
                // wait briefly for progress to start.
                let mut generation_advanced_at: Option<tokio::time::Instant> = None;

                loop {
                    let gen_advanced = self.diagnostics_generation(uri).await > snapshot
                        && self.is_diagnostics_version_current(uri).await;

                    if gen_advanced && generation_advanced_at.is_none() {
                        generation_advanced_at = Some(tokio::time::Instant::now());
                    }

                    // If diagnostics arrived but no progress tokens, use
                    // load_aware_grace for the progress grace window.
                    if generation_advanced_at.is_some() && !ever_active {
                        let progress_started = load_aware_grace(
                            &mut || self.sample_monitor(),
                            PREAMBLE_THRESHOLD,
                            Some(Duration::from_secs(2)),
                            &self.progress_notify,
                            || self.progress_active(),
                            || async { self.progress_active() },
                        )
                        .await;

                        if !progress_started {
                            // No progress tokens arrived — return what we have
                            return DiagnosticsWaitResult::Diagnostics;
                        }
                        ever_active = true;
                        continue;
                    }

                    match monitor.poll() {
                        ActivityState::Dead => return DiagnosticsWaitResult::Nothing,
                        ActivityState::Active => {
                            ever_active = true;
                        }
                        ActivityState::Idle if ever_active => {
                            // Active → Idle: the full progress cycle completed.
                            // Check for diagnostics one more time.
                            if self.diagnostics_generation(uri).await > snapshot {
                                return DiagnosticsWaitResult::Diagnostics;
                            }
                            debug!("TokenMonitor: Active \u{2192} Idle without new diagnostics");
                            return DiagnosticsWaitResult::Nothing;
                        }
                        ActivityState::Idle => {}
                    }

                    // Event-driven wake + failure detection
                    tokio::select! {
                        () = self.diagnostics_notify.notified() => continue,
                        () = self.progress_notify.notified() => continue,
                        () = tokio::time::sleep(POLL_INTERVAL) => {}
                    }

                    // Failure detection (progress-aware)
                    if let Some((delta, state)) = self.sample_monitor() {
                        if state == catenary_proc::ProcessState::Dead {
                            return DiagnosticsWaitResult::Nothing;
                        }
                        if state == catenary_proc::ProcessState::Running
                            && delta > 0
                            && !self.progress_active()
                        {
                            budget -= i64::try_from(delta).unwrap_or(budget);
                        }
                    }

                    if budget <= 0 {
                        debug!("TokenMonitor: tick budget exhausted");
                        return DiagnosticsWaitResult::Nothing;
                    }
                    if tokio::time::Instant::now() >= wall_deadline {
                        debug!("TokenMonitor: safety cap reached");
                        return DiagnosticsWaitResult::Nothing;
                    }
                }
            }
        }
    }

    /// Returns the command used to spawn this server (e.g., "rust-analyzer").
    pub fn server_command(&self) -> &str {
        &self.server_command
    }

    /// Returns the server version from the LSP `initialize` response.
    pub fn server_version(&self) -> Option<&str> {
        self.server_version.as_deref()
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
    ///
    /// Checks `ServerState::Ready` and confirms the process is idle
    /// via `ProcessMonitor`. A server that is Ready and Sleeping has
    /// finished initialization and is waiting for requests.
    ///
    /// During warmup (first 3 seconds), requires the process to be
    /// Sleeping to avoid premature readiness before indexing starts.
    /// After warmup, Ready state alone is sufficient.
    pub fn is_ready(&self) -> bool {
        if self.server_state() != ServerState::Ready || !self.is_alive() {
            return false;
        }

        // During warmup, verify the server is actually idle
        if self.spawn_time.elapsed() < Duration::from_secs(3) {
            let Some((_delta, state)) = self.sample_monitor() else {
                return false;
            };
            return state == catenary_proc::ProcessState::Sleeping;
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
    /// Uses load-aware failure detection instead of wall-clock timeouts.
    /// For servers with `$/progress`, wakes on progress state transitions.
    /// The failure threshold only counts unexplained CPU consumption.
    ///
    /// For non-progress servers that get set to `Busy` (e.g. after a
    /// workspace folder change), detects activity settle: if the server is
    /// `Busy`, `Sleeping`, and has flat ticks for consecutive samples,
    /// it has finished processing the notification and is transitioned back
    /// to `Ready`.
    ///
    /// Returns `true` if ready, `false` if server died or is stuck.
    pub async fn wait_ready(&self) -> bool {
        /// Consecutive flat+sleeping samples required to settle back to Ready.
        const SETTLE_SAMPLES: u32 = 3;

        // Stuck servers have already exhausted patience — don't wait again.
        // Take an opportunistic sample to keep the baseline fresh for
        // try_idle_recover() on the next check_server_health() call.
        if self.server_state() == ServerState::Stuck {
            let _ = self.sample_monitor();
            return false;
        }

        let flat_count = AtomicU32::new(0);

        let ready = load_aware_grace(
            &mut || self.sample_monitor(),
            READY_THRESHOLD,
            None, // Use default 5-minute safety cap
            &self.state_notify,
            || self.progress_active(),
            || async {
                if self.is_ready() {
                    return true;
                }

                // Activity settle for non-progress servers: if state is
                // Busy and the process is sleeping with flat ticks,
                // the server accepted the notification and went idle.
                if self.server_state() == ServerState::Busy && self.is_alive() {
                    if let Some((delta, process_state)) = self.sample_monitor() {
                        if process_state == catenary_proc::ProcessState::Sleeping && delta == 0 {
                            let count = flat_count.fetch_add(1, Ordering::SeqCst) + 1;
                            if count >= SETTLE_SAMPLES {
                                tracing::debug!(
                                    "wait_ready: activity settle — non-progress server \
                                     idle for {count} samples, transitioning to Ready"
                                );
                                self.state
                                    .store(ServerState::Ready.as_u8(), Ordering::SeqCst);
                                self.state_notify.notify_waiters();
                                return true;
                            }
                        } else {
                            flat_count.store(0, Ordering::SeqCst);
                        }
                    }
                } else {
                    flat_count.store(0, Ordering::SeqCst);
                }

                false
            },
        )
        .await;

        // Patience exhausted but process is still alive — mark as stuck
        // so future calls skip the full wait.
        if !ready && self.is_alive() {
            debug!("wait_ready: patience exhausted, server still alive — marking as Stuck");
            self.state
                .store(ServerState::Stuck.as_u8(), Ordering::SeqCst);
            self.state_notify.notify_waiters();
        }

        ready
    }

    /// Lightweight idle probe for `Stuck` servers.
    ///
    /// If the server is `Stuck`, alive, and the process is sleeping with
    /// flat CPU ticks, transitions to `Ready` and returns `true`.
    /// Returns `false` in all other cases. Costs one process sample (~1ms).
    pub fn try_idle_recover(&self) -> bool {
        if self.server_state() != ServerState::Stuck || !self.is_alive() {
            return false;
        }

        if let Some((delta, process_state)) = self.sample_monitor()
            && process_state == catenary_proc::ProcessState::Sleeping
            && delta == 0
        {
            debug!("try_idle_recover: stuck server is idle — transitioning to Ready");
            self.state
                .store(ServerState::Ready.as_u8(), Ordering::SeqCst);
            self.state_notify.notify_waiters();
            return true;
        }

        false
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // We can't await a graceful LSP shutdown here because drop is sync.
        // But we MUST ensure the child process doesn't become a zombie.
        let _ = self.child.start_kill();
    }
}

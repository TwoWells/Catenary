// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

use anyhow::Result;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, info};

use super::connection::Connection;
use super::inbox::{Inbox, ServerInbox};
use super::params;
use super::server::LspServer;
use super::state::{ServerState, ServerStatus};
use super::wait::load_aware_grace;
use crate::session::MessageLog;

/// Cached diagnostics for a file: `(version, diagnostics)`.
///
/// `version` is the document version from `publishDiagnostics`, if the
/// server includes it. Used by [`DiagnosticsStrategy::Version`] to
/// match diagnostics to a specific document change.
pub type DiagnosticsCache = Arc<std::sync::Mutex<HashMap<String, (Option<i32>, Vec<Value>)>>>;

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

/// CPU tick threshold for `wait_ready`: 1000 ticks = 10 CPU-seconds.
const READY_THRESHOLD: u64 = 1000;

/// Poll interval for diagnostics wait main loops.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Wall-clock safety cap (5 minutes) for diagnostics wait.
const SAFETY_CAP: Duration = Duration::from_secs(300);

/// Manages communication with an LSP server process.
pub struct LspClient {
    connection: Connection,

    // Grouped server state
    inbox: Arc<ServerInbox>,

    // Client-local state (not shared with reader)
    encoding: String,
    /// Time when this client was spawned.
    spawn_time: Instant,
    /// Whether the server supports dynamic workspace folder changes
    /// (both `supported` and `change_notifications` are advertised).
    supports_workspace_folders: bool,
    /// Logged once when a server is detected as lacking diagnostics support.
    logged_no_diagnostics_support: AtomicBool,
    /// Last document version sent via `did_open`/`did_change` per URI.
    /// Used to detect stale diagnostics from prior document versions.
    last_sent_version: Arc<Mutex<HashMap<String, i32>>>,
    /// Whether the server advertised `textDocumentSync.save` support.
    wants_did_save: bool,
    /// The command used to spawn this server (e.g., "rust-analyzer").
    server_command: String,
    /// Server version from the `initialize` response (`ServerInfo.version`).
    /// Populated after `initialize()` completes; `None` if the server
    /// did not report a version.
    server_version: Option<String>,
    /// Server profile constructed during `initialize()`.
    /// `None` before `initialize()` completes.
    lsp_server: Option<Arc<LspServer>>,
    /// Parent message ID for causation tracking (set before tool dispatch).
    parent_id: Option<i64>,
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
        message_log: Arc<MessageLog>,
        settings: Option<serde_json::Value>,
    ) -> Result<Self> {
        Self::spawn_inner(
            program,
            args,
            language,
            message_log,
            Stdio::inherit(),
            settings,
        )
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
        message_log: Arc<MessageLog>,
    ) -> Result<Self> {
        Self::spawn_inner(program, args, language, message_log, Stdio::null(), None)
    }

    fn spawn_inner(
        program: &str,
        args: &[&str],
        language: &str,
        message_log: Arc<MessageLog>,
        stderr: Stdio,
        settings: Option<serde_json::Value>,
    ) -> Result<Self> {
        let inbox = Arc::new(ServerInbox::new(language.to_string(), settings));

        let connection = Connection::new(
            program,
            args,
            stderr,
            inbox.clone(),
            language.to_string(),
            message_log,
            program,
        )?;

        Ok(Self {
            connection,
            inbox,
            encoding: "utf-16".to_string(), // Default per spec
            spawn_time: Instant::now(),
            supports_workspace_folders: false,
            logged_no_diagnostics_support: AtomicBool::new(false),
            last_sent_version: Arc::new(Mutex::new(HashMap::new())),
            wants_did_save: false,
            server_command: program.to_string(),
            server_version: None,
            lsp_server: None,
            parent_id: None,
        })
    }

    /// Samples the server process via the persistent `ProcessMonitor`.
    ///
    /// Returns [`ProcessDelta`](catenary_proc::ProcessDelta) with per-counter
    /// deltas since the last sample. Returns `None` if the process is gone
    /// or monitoring is unavailable.
    fn sample_monitor(&self) -> Option<catenary_proc::ProcessDelta> {
        self.connection.sample_monitor()
    }

    /// Returns whether the server has active `$/progress` tokens.
    ///
    /// Checks the actual progress tracker instead of using `ServerState`
    /// as a proxy. `ServerState::Busy` can be set proactively (e.g., after
    /// `workspace/didChangeWorkspaceFolders`) without actual `$/progress`
    /// tokens, which would prevent the failure threshold from draining.
    fn progress_active(&self) -> bool {
        self.inbox.is_progress_active()
    }

    /// Sets the parent message ID for causation tracking.
    ///
    /// All subsequent requests and notifications will carry this parent ID
    /// until it is changed or cleared.
    pub const fn set_parent_id(&mut self, parent_id: Option<i64>) {
        self.parent_id = parent_id;
    }

    /// Sends a request and waits for the response.
    ///
    /// Delegates to [`Connection::request`] for transport and failure
    /// detection, returning the raw JSON response.
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.connection
            .request(method, params, self.parent_id)
            .await
    }

    /// Sends a notification (no response expected).
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.connection.notify(method, params, self.parent_id).await
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
    ) -> Result<Value> {
        let workspace_folders: Vec<(String, String)> = roots
            .iter()
            .map(|root| {
                let uri = format!("file://{}", root.display());
                let name = root.file_name().map_or_else(
                    || "workspace".to_string(),
                    |s| s.to_string_lossy().to_string(),
                );
                (uri, name)
            })
            .collect();

        let folder_refs: Vec<(&str, &str)> = workspace_folders
            .iter()
            .map(|(uri, name)| (uri.as_str(), name.as_str()))
            .collect();

        let init_params = params::initialize(
            std::process::id(),
            &folder_refs,
            initialization_options.as_ref(),
        );

        let raw = self.request("initialize", init_params).await?;

        let caps = raw
            .get("capabilities")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::default()));

        // Extract negotiated encoding
        if let Some(enc) = super::extract::position_encoding(&caps) {
            self.encoding = enc.to_string();
            debug!("Negotiated position encoding: {}", self.encoding);
        } else {
            debug!("Server did not specify position encoding, defaulting to UTF-16");
            self.encoding = "utf-16".to_string();
        }

        // Extract workspace folders capability
        self.supports_workspace_folders = super::extract::supports_workspace_folders(&caps);
        debug!(
            "Server workspace folders support: {}",
            self.supports_workspace_folders
        );

        // Extract textDocumentSync.save capability
        self.wants_did_save = super::extract::wants_did_save(&caps);
        debug!(
            "[{}] server wants didSave: {}",
            self.inbox.language, self.wants_did_save
        );

        // Store server info and construct server profile
        self.server_version = super::extract::server_version(&raw).map(str::to_string);
        let server = Arc::new(LspServer::new(caps));
        self.inbox.set_lsp_server(Arc::clone(&server));
        self.lsp_server = Some(server);

        // Send initialized notification
        self.notify("initialized", json!({})).await?;

        // Trigger configuration pull: servers that support the pull model
        // (workspace/configuration) will request specific sections; servers
        // using the legacy push model read settings directly from this
        // notification.
        let settings = self
            .inbox
            .settings()
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        self.notify(
            "workspace/didChangeConfiguration",
            json!({"settings": settings}),
        )
        .await?;

        // Mark as ready (server may later report progress if indexing)
        self.inbox
            .state
            .store(ServerState::Ready.as_u8(), Ordering::SeqCst);

        Ok(raw)
    }

    /// Returns the negotiated position encoding.
    pub fn encoding(&self) -> &str {
        &self.encoding
    }

    /// Returns the server capabilities from the `initialize` response.
    ///
    /// Returns an empty object before `initialize()` completes.
    pub fn capabilities(&self) -> &Value {
        static EMPTY: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        self.lsp_server.as_ref().map_or_else(
            || EMPTY.get_or_init(|| Value::Object(serde_json::Map::new())),
            |s| s.capabilities(),
        )
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
    pub async fn did_open(
        &self,
        uri: &str,
        language_id: &str,
        version: i32,
        text: &str,
    ) -> Result<()> {
        self.last_sent_version
            .lock()
            .await
            .insert(uri.to_string(), version);
        self.notify(
            "textDocument/didOpen",
            params::did_open(uri, language_id, version, text),
        )
        .await
    }

    /// Notifies the LSP server that a document changed.
    ///
    /// # Errors
    ///
    /// Returns an error if the notification fails.
    pub async fn did_change(&self, uri: &str, version: i32, text: &str) -> Result<()> {
        self.last_sent_version
            .lock()
            .await
            .insert(uri.to_string(), version);
        self.notify(
            "textDocument/didChange",
            params::did_change(uri, version, text),
        )
        .await
    }

    /// Notifies the LSP server that a document was saved.
    ///
    /// This triggers flycheck (e.g., `cargo check`) on servers that only
    /// run diagnostics on save, like rust-analyzer.
    ///
    /// # Errors
    ///
    /// Returns an error if the notification fails.
    pub async fn did_save(&self, uri: &str) -> Result<()> {
        self.notify("textDocument/didSave", params::did_save(uri))
            .await
    }

    /// Notifies the LSP server that a document was closed.
    ///
    /// # Errors
    ///
    /// Returns an error if the notification fails.
    pub async fn did_close(&self, uri: &str) -> Result<()> {
        self.notify("textDocument/didClose", params::did_close(uri))
            .await
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
        added: &[(&str, &str)],
        removed: &[(&str, &str)],
    ) -> Result<()> {
        if !added.is_empty() && self.server_state() == ServerState::Ready {
            self.inbox
                .state
                .store(ServerState::Busy.as_u8(), Ordering::SeqCst);
        }

        self.notify(
            "workspace/didChangeWorkspaceFolders",
            params::did_change_workspace_folders(added, removed),
        )
        .await
    }

    /// Gets hover information (signature, documentation) for a position.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn hover(&self, uri: &str, line: u32, character: u32) -> Result<Value> {
        self.request("textDocument/hover", params::hover(uri, line, character))
            .await
    }

    /// Tests whether a position is a renameable symbol.
    ///
    /// Returns a non-null `Value` for symbols, `Value::Null` for keywords
    /// and non-symbol positions. Used as a cheap discriminator before full
    /// enrichment in the rg-bootstrap path.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn prepare_rename(&self, uri: &str, line: u32, character: u32) -> Result<Value> {
        self.request(
            "textDocument/prepareRename",
            params::prepare_rename(uri, line, character),
        )
        .await
    }

    /// Gets the definition location for a symbol.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn definition(&self, uri: &str, line: u32, character: u32) -> Result<Value> {
        self.request(
            "textDocument/definition",
            params::definition(uri, line, character),
        )
        .await
    }

    /// Gets the type definition location for a symbol.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn type_definition(&self, uri: &str, line: u32, character: u32) -> Result<Value> {
        self.request(
            "textDocument/typeDefinition",
            params::type_definition(uri, line, character),
        )
        .await
    }

    /// Gets implementation locations for a symbol.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn implementation(&self, uri: &str, line: u32, character: u32) -> Result<Value> {
        self.request(
            "textDocument/implementation",
            params::implementation(uri, line, character),
        )
        .await
    }

    /// Gets all references to a symbol.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn references(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<Value> {
        self.request(
            "textDocument/references",
            params::references(uri, line, character, include_declaration),
        )
        .await
    }

    /// Gets document symbols (outline) for a file.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn document_symbols(&self, uri: &str) -> Result<Value> {
        self.request("textDocument/documentSymbol", params::document_symbols(uri))
            .await
    }

    /// Searches for symbols across the workspace.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn workspace_symbols(&self, query: &str) -> Result<Value> {
        self.request("workspace/symbol", params::workspace_symbols(query))
            .await
    }

    /// Resolves additional properties (e.g. `location.range`) for a workspace symbol.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn workspace_symbol_resolve(&self, symbol: &Value) -> Result<Value> {
        self.request("workspaceSymbol/resolve", symbol.clone())
            .await
    }

    /// Returns whether the server advertises `workspaceSymbol/resolve` support.
    pub fn supports_workspace_symbol_resolve(&self) -> bool {
        super::extract::workspace_symbol_resolve_provider(self.capabilities())
    }

    /// Returns whether the server advertises `diagnosticProvider` (pull model).
    pub fn pulls_diagnostics(&self) -> bool {
        self.lsp_server
            .as_ref()
            .is_some_and(|s| s.pulls_diagnostics())
    }

    /// Returns whether the server advertises `typeHierarchyProvider`.
    pub fn supports_type_hierarchy(&self) -> bool {
        super::extract::has_type_hierarchy_provider(self.capabilities())
    }

    /// Prepares call hierarchy for a position.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn prepare_call_hierarchy(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Value> {
        self.request(
            "textDocument/prepareCallHierarchy",
            params::prepare_call_hierarchy(uri, line, character),
        )
        .await
    }

    /// Gets incoming calls to a call hierarchy item.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn incoming_calls(&self, item: &Value) -> Result<Value> {
        self.request("callHierarchy/incomingCalls", params::incoming_calls(item))
            .await
    }

    /// Gets outgoing calls from a call hierarchy item.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn outgoing_calls(&self, item: &Value) -> Result<Value> {
        self.request("callHierarchy/outgoingCalls", params::outgoing_calls(item))
            .await
    }

    /// Prepares type hierarchy for a position.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn prepare_type_hierarchy(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Value> {
        self.request(
            "textDocument/prepareTypeHierarchy",
            params::prepare_type_hierarchy(uri, line, character),
        )
        .await
    }

    /// Gets supertypes of a type hierarchy item.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn supertypes(&self, item: &Value) -> Result<Value> {
        self.request("typeHierarchy/supertypes", params::supertypes(item))
            .await
    }

    /// Gets subtypes of a type hierarchy item.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn subtypes(&self, item: &Value) -> Result<Value> {
        self.request("typeHierarchy/subtypes", params::subtypes(item))
            .await
    }

    /// Gets code actions (quick fixes) for a range.
    ///
    /// Bakes in `only: ["quickfix"]` because the only caller (notify.rs)
    /// always wants quickfixes.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn code_action(
        &self,
        uri: &str,
        start_line: u32,
        start_char: u32,
        end_line: u32,
        end_char: u32,
        diagnostics: &[Value],
    ) -> Result<Value> {
        let params = json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": start_line, "character": start_char },
                "end": { "line": end_line, "character": end_char }
            },
            "context": {
                "diagnostics": diagnostics,
                "only": ["quickfix"]
            }
        });
        self.request("textDocument/codeAction", params).await
    }

    /// Pulls diagnostics from the server via `textDocument/diagnostic`.
    ///
    /// Returns the diagnostics array from the response, or an empty
    /// vec on error/timeout.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails or times out.
    pub async fn pull_diagnostics(&self, uri: &str) -> Result<Vec<Value>> {
        let result = self
            .request(
                "textDocument/diagnostic",
                params::text_document_diagnostic(uri),
            )
            .await?;
        Ok(super::extract::document_diagnostic_report(&result))
    }

    /// Gets cached diagnostics for a specific URI.
    pub fn get_diagnostics(&self, uri: &str) -> Vec<Value> {
        let cache = self
            .inbox
            .diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    pub(crate) fn cached_diagnostics_version(&self, uri: &str) -> Option<i32> {
        let cache = self
            .inbox
            .diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.get(uri).and_then(|(version, _)| *version)
    }

    /// Returns whether cached diagnostics match the last-sent document version.
    ///
    /// Returns `true` (assume current) when the server doesn't publish version
    /// info or when no version has been tracked for this URI — we can't
    /// distinguish stale from fresh without version data.
    async fn is_diagnostics_version_current(&self, uri: &str) -> bool {
        if !self.inbox.publishes_version.load(Ordering::SeqCst) {
            return true;
        }
        let sent = self.last_sent_version.lock().await;
        let Some(sent_v) = sent.get(uri).copied() else {
            return true;
        };
        drop(sent);
        let cached_v = self.cached_diagnostics_version(uri);
        cached_v.is_some_and(|v| v >= sent_v)
    }

    /// Returns the current diagnostics generation for a URI.
    ///
    /// Callers should snapshot this *before* sending a change notification,
    /// then pass the snapshot to [`wait_for_diagnostics_update`] to ensure
    /// the returned diagnostics reflect that specific change.
    pub fn diagnostics_generation(&self, uri: &str) -> u64 {
        let generations = self
            .inbox
            .diagnostics_generation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

        let sends_progress = self.lsp_server.as_ref().is_some_and(|s| s.sends_progress());
        if sends_progress {
            Some(DiagnosticsStrategy::TokenMonitor)
        } else if self.inbox.publishes_version.load(Ordering::SeqCst) {
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
        let sends_progress = self.lsp_server.as_ref().is_some_and(|s| s.sends_progress());
        if self.inbox.publishes_version.load(Ordering::SeqCst) || sends_progress {
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
                self.inbox.language
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
        self.connection.pid()
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
        uri: &str,
        snapshot: u64,
    ) -> DiagnosticsWaitResult {
        use super::diagnostics::{ActivityState, DiagnosticsStrategy, ProgressMonitor};

        // ── Grace period ─────────────────────────────────────────────
        // For servers that haven't published diagnostics yet, wait for
        // the first publishDiagnostics using load-aware failure detection.
        let pushes_diagnostics = self
            .lsp_server
            .as_ref()
            .is_some_and(|s| s.pushes_diagnostics());
        if !pushes_diagnostics {
            let grace_ok = load_aware_grace(
                &mut || self.sample_monitor(),
                PREAMBLE_THRESHOLD,
                Some(Duration::from_secs(10)),
                &self.inbox.diagnostics_notify,
                || self.progress_active(),
                || async { self.diagnostics_generation(uri) > snapshot },
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
                    () = self.inbox.capability_notify.notified() => {}
                    () = tokio::time::sleep(POLL_INTERVAL) => {}
                }
            }
        };
        debug!(
            "Diagnostics strategy: {:?} (sends_progress={}, publishes_version={})",
            strategy,
            self.lsp_server.as_ref().is_some_and(|s| s.sends_progress()),
            self.inbox.publishes_version.load(Ordering::SeqCst),
        );

        let wall_deadline = tokio::time::Instant::now() + SAFETY_CAP;
        let mut budget: i64 = i64::try_from(DIAGNOSTICS_THRESHOLD).unwrap_or(1000);

        // ── Main wait loops ──────────────────────────────────────────
        match strategy {
            DiagnosticsStrategy::Version => {
                // Wait for publishDiagnostics with version >= our change.
                loop {
                    if self.diagnostics_generation(uri) > snapshot
                        && self.is_diagnostics_version_current(uri).await
                    {
                        return DiagnosticsWaitResult::Diagnostics;
                    }

                    // Event-driven wake + failure detection
                    tokio::select! {
                        () = self.inbox.diagnostics_notify.notified() => {
                            // Check condition at top of loop
                            continue;
                        }
                        () = tokio::time::sleep(POLL_INTERVAL) => {}
                    }

                    // Failure detection
                    if let Some(d) = self.sample_monitor() {
                        if d.state == catenary_proc::ProcessState::Dead {
                            return DiagnosticsWaitResult::Nothing;
                        }
                        let delta = d.delta_utime + d.delta_stime;
                        if d.state == catenary_proc::ProcessState::Running
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
                let mut monitor = super::diagnostics::TokenMonitor::new(
                    self.inbox.state.clone(),
                    self.connection.alive_flag(),
                );
                let mut ever_active = false;

                // Progress grace: if diagnostics arrive before progress tokens,
                // wait briefly for progress to start.
                let mut generation_advanced_at: Option<tokio::time::Instant> = None;

                loop {
                    let gen_advanced = self.diagnostics_generation(uri) > snapshot
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
                            &self.inbox.progress_notify,
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
                            if self.diagnostics_generation(uri) > snapshot {
                                return DiagnosticsWaitResult::Diagnostics;
                            }
                            debug!("TokenMonitor: Active \u{2192} Idle without new diagnostics");
                            return DiagnosticsWaitResult::Nothing;
                        }
                        ActivityState::Idle => {}
                    }

                    // Event-driven wake + failure detection
                    tokio::select! {
                        () = self.inbox.diagnostics_notify.notified() => continue,
                        () = self.inbox.progress_notify.notified() => continue,
                        () = tokio::time::sleep(POLL_INTERVAL) => {}
                    }

                    // Failure detection (progress-aware)
                    if let Some(d) = self.sample_monitor() {
                        if d.state == catenary_proc::ProcessState::Dead {
                            return DiagnosticsWaitResult::Nothing;
                        }
                        let delta = d.delta_utime + d.delta_stime;
                        if d.state == catenary_proc::ProcessState::Running
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
        &self.inbox.language
    }

    /// Returns whether the server supports dynamic workspace folder changes.
    pub const fn supports_workspace_folders(&self) -> bool {
        self.supports_workspace_folders
    }

    /// Returns whether the LSP server process is still running.
    pub fn is_alive(&self) -> bool {
        self.connection.is_alive()
    }

    /// Returns the current server state.
    pub fn server_state(&self) -> ServerState {
        ServerState::from_u8(self.inbox.state.load(Ordering::SeqCst))
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
            let Some(d) = self.sample_monitor() else {
                return false;
            };
            return d.state == catenary_proc::ProcessState::Sleeping;
        }

        true
    }

    /// Returns detailed status for this server.
    pub fn status(&self, language: String) -> ServerStatus {
        let (title, message, percentage) = {
            let progress = self
                .inbox
                .progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            &self.inbox.state_notify,
            || self.progress_active(),
            || async {
                if self.is_ready() {
                    return true;
                }

                // Activity settle for non-progress servers: if state is
                // Busy and the process is sleeping with flat ticks,
                // the server accepted the notification and went idle.
                if self.server_state() == ServerState::Busy && self.is_alive() {
                    if let Some(d) = self.sample_monitor() {
                        if d.state == catenary_proc::ProcessState::Sleeping
                            && d.delta_utime + d.delta_stime == 0
                        {
                            let count = flat_count.fetch_add(1, Ordering::SeqCst) + 1;
                            if count >= SETTLE_SAMPLES {
                                tracing::debug!(
                                    "wait_ready: activity settle — non-progress server \
                                     idle for {count} samples, transitioning to Ready"
                                );
                                self.inbox
                                    .state
                                    .store(ServerState::Ready.as_u8(), Ordering::SeqCst);
                                self.inbox.state_notify.notify_waiters();
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
            self.inbox
                .state
                .store(ServerState::Stuck.as_u8(), Ordering::SeqCst);
            self.inbox.state_notify.notify_waiters();
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

        if let Some(d) = self.sample_monitor()
            && d.state == catenary_proc::ProcessState::Sleeping
            && d.delta_utime + d.delta_stime == 0
        {
            debug!("try_idle_recover: stuck server is idle — transitioning to Ready");
            self.inbox
                .state
                .store(ServerState::Ready.as_u8(), Ordering::SeqCst);
            self.inbox.state_notify.notify_waiters();
            return true;
        }

        false
    }
}

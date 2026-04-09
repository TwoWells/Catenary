// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Shared server state and notification dispatch.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Notify;
use tracing::{debug, trace, warn};

use super::client::DiagnosticsCache;
use super::extract;
use super::protocol::RpcError;
use super::server::LspServer;
use super::state::{ProgressTracker, ServerState};

/// Receives server-initiated messages from the Connection reader loop.
///
/// All methods are synchronous. The async byte-reading lives in
/// Connection; by the time these methods are called, the message
/// is already parsed.
pub trait Inbox: Send + Sync {
    /// Handle a server notification (no response needed).
    fn on_notification(&self, method: &str, params: &Value);

    /// Handle a server request (response required).
    ///
    /// Returns `Ok(result)` for a success response or `Err(RpcError)`
    /// for an error response. Connection builds the JSON-RPC envelope.
    fn on_request(&self, method: &str, params: &Value) -> Result<Value, RpcError>;

    /// Handle reader loop shutdown (server connection lost).
    ///
    /// Called after the `alive` flag is set to `false`. Updates internal
    /// state and wakes any waiters blocked on diagnostics or state changes.
    fn on_shutdown(&self);

    /// Whether the server is actively reporting progress.
    ///
    /// Used by `Connection::request` to pause failure detection budget
    /// drain during explained work (e.g., indexing, flycheck).
    fn is_progress_active(&self) -> bool;

    /// Returns a reference to the state-change notifier.
    ///
    /// Used by `Connection::request` to wait for server settle after
    /// `ContentModified` instead of a fixed sleep.
    fn state_notify(&self) -> &Notify;
}

/// Shared server state for notification dispatch.
///
/// Groups all state that the reader task needs to update when processing
/// LSP notifications. Passed as `Arc<ServerInbox>` to the reader task.
pub struct ServerInbox {
    // Diagnostics
    pub(crate) diagnostics: DiagnosticsCache,
    pub(crate) diagnostics_generation: Arc<Mutex<HashMap<String, u64>>>,
    pub(crate) diagnostics_notify: Arc<Notify>,

    // Capability discovery
    pub(crate) capability_notify: Arc<Notify>,

    // Progress
    pub(crate) progress: Arc<Mutex<ProgressTracker>>,
    pub(crate) progress_notify: Arc<Notify>,

    // Server state
    pub(crate) state: Arc<AtomicU8>,
    pub(crate) state_notify: Arc<Notify>,

    // Observation flags
    pub(crate) publishes_version: Arc<AtomicBool>,

    // Server profile (set after initialize completes)
    lsp_server: OnceLock<Arc<LspServer>>,

    // Identity
    pub(crate) language: String,

    // Configuration
    settings: Option<Value>,
}

impl ServerInbox {
    /// Creates a new `ServerInbox` with default state.
    pub(crate) fn new(language: String, settings: Option<Value>) -> Self {
        Self {
            diagnostics: Arc::new(Mutex::new(HashMap::new())),
            diagnostics_generation: Arc::new(Mutex::new(HashMap::new())),
            diagnostics_notify: Arc::new(Notify::new()),
            capability_notify: Arc::new(Notify::new()),
            progress: Arc::new(Mutex::new(ProgressTracker::new())),
            progress_notify: Arc::new(Notify::new()),
            state: Arc::new(AtomicU8::new(ServerState::Initializing.as_u8())),
            state_notify: Arc::new(Notify::new()),
            publishes_version: Arc::new(AtomicBool::new(false)),
            lsp_server: OnceLock::new(),
            language,
            settings,
        }
    }

    /// Returns the server settings, if configured.
    pub(crate) const fn settings(&self) -> Option<&Value> {
        self.settings.as_ref()
    }

    /// Sets the server profile after the `initialize` handshake completes.
    ///
    /// Called once from `LspClient::initialize()`. Subsequent calls are
    /// no-ops (the `OnceLock` ignores them).
    pub(crate) fn set_lsp_server(&self, server: Arc<LspServer>) {
        let _ = self.lsp_server.set(server);
    }

    /// Returns the server profile, if available.
    #[allow(dead_code, reason = "Phase 1a-02 will use for pull diagnostics")]
    pub(crate) fn lsp_server(&self) -> Option<&Arc<LspServer>> {
        self.lsp_server.get()
    }
}

/// Resolves a `workspace/configuration` section path against settings.
///
/// Splits `section` on `.` and traverses the JSON object tree.
/// Returns `{}` if settings are `None`, section is `None`, or the path
/// doesn't match.
fn resolve_section(settings: Option<&Value>, section: Option<&str>) -> Value {
    let (Some(mut current), Some(section)) = (settings, section) else {
        return Value::Object(serde_json::Map::new());
    };
    for key in section.split('.') {
        match current.get(key) {
            Some(child) => current = child,
            None => return Value::Object(serde_json::Map::new()),
        }
    }
    current.clone()
}

impl Inbox for ServerInbox {
    #[allow(clippy::too_many_lines, reason = "match dispatcher with per-arm logic")]
    fn on_notification(&self, method: &str, params: &Value) {
        match method {
            "textDocument/publishDiagnostics" => {
                let Some(uri) = extract::publish_diagnostics_uri(params) else {
                    warn!("publishDiagnostics missing uri");
                    return;
                };
                let version = extract::publish_diagnostics_version(params);
                let diagnostics = extract::publish_diagnostics_diagnostics(params);

                debug!(
                    "Received {} diagnostics for {:?} (version={:?})",
                    diagnostics.len(),
                    uri,
                    version,
                );

                // Track whether server provides version in diagnostics
                if version.is_some() && !self.publishes_version.swap(true, Ordering::SeqCst) {
                    self.capability_notify.notify_waiters();
                }

                let mut cache = self
                    .diagnostics
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                cache.insert(uri.to_string(), (version, diagnostics));
                drop(cache);

                // Bump generation counter and wake waiters
                let mut generations = self
                    .diagnostics_generation
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let counter = generations.entry(uri.to_string()).or_insert(0);
                *counter += 1;
                drop(generations);
                self.diagnostics_notify.notify_waiters();
            }
            "$/progress" => {
                let Some(token_value) = extract::progress_token(params) else {
                    warn!("$/progress missing token");
                    return;
                };
                let token_str = token_value
                    .as_str()
                    .map_or_else(|| token_value.to_string(), str::to_string);

                let mut tracker = self
                    .progress
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                tracker.update(&token_str, &params["value"]);

                // Update server profile based on progress kind
                if let Some(server) = self.lsp_server.get() {
                    let kind = params["value"]["kind"].as_str();
                    match kind {
                        Some("begin") => {
                            if server.on_progress_begin() {
                                self.capability_notify.notify_waiters();
                            }
                        }
                        Some("end") => {
                            server.on_progress_end();
                        }
                        _ => {}
                    }
                }

                // Update state based on progress.
                // The Dead guard is the only exclusion — Stuck servers
                // that send progress are naturally recovered here
                // (transitioned to Busy/Ready like any other state).
                let current_state = ServerState::from_u8(self.state.load(Ordering::SeqCst));
                if current_state != ServerState::Dead {
                    if tracker.is_busy() {
                        self.state
                            .store(ServerState::Busy.as_u8(), Ordering::SeqCst);
                        if tracker.broadcast_changed()
                            && let Some(p) = tracker.primary_progress()
                        {
                            debug!("Progress: {} {}%", p.title, p.percentage.unwrap_or(0));
                        }
                    } else {
                        self.state
                            .store(ServerState::Ready.as_u8(), Ordering::SeqCst);
                        debug!("Server ready (progress completed)");
                    }
                    // Fire notifies after state update
                    self.progress_notify.notify_waiters();
                    self.state_notify.notify_waiters();
                }
            }
            "window/logMessage" | "window/showMessage" => {
                if let Some(message) = params.get("message").and_then(|m| m.as_str()) {
                    debug!("LSP server message: {}", message);
                }
            }
            _ => {
                trace!("Ignoring notification: {} params={}", method, params);
            }
        }
    }

    fn on_request(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "workspace/configuration" => {
                let items = params.get("items").and_then(Value::as_array);
                let item_count = items.map_or(1, Vec::len);
                let results: Vec<Value> = (0..item_count)
                    .map(|i| {
                        let section = items
                            .and_then(|arr| arr.get(i))
                            .and_then(|item| item.get("section"))
                            .and_then(Value::as_str);
                        resolve_section(self.settings.as_ref(), section)
                    })
                    .collect();
                Ok(Value::Array(results))
            }
            "window/workDoneProgress/create"
            | "client/registerCapability"
            | "client/unregisterCapability"
            | "window/showMessageRequest" => Ok(Value::Null),
            _ => Err(RpcError {
                code: -32601,
                message: format!("Method '{method}' not supported by client"),
            }),
        }
    }

    fn on_shutdown(&self) {
        self.state
            .store(ServerState::Dead.as_u8(), Ordering::SeqCst);
        if let Ok(mut progress) = self.progress.lock() {
            progress.clear();
        }
        self.diagnostics_notify.notify_waiters();
        self.state_notify.notify_waiters();
    }

    fn is_progress_active(&self) -> bool {
        self.progress
            .try_lock()
            .map_or(true, |tracker| tracker.is_busy())
    }

    fn state_notify(&self) -> &Notify {
        &self.state_notify
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_inbox() -> ServerInbox {
        ServerInbox::new("test".to_string(), None)
    }

    #[test]
    fn resolve_section_traverses_dot_path() {
        let settings = json!({
            "python": {
                "analysis": {
                    "exclude": ["**/target"],
                    "extraPaths": []
                },
                "pythonPath": "/usr/bin/python3"
            }
        });
        assert_eq!(
            resolve_section(Some(&settings), Some("python.analysis")),
            json!({"exclude": ["**/target"], "extraPaths": []})
        );
        assert_eq!(
            resolve_section(Some(&settings), Some("python.pythonPath")),
            json!("/usr/bin/python3")
        );
        assert_eq!(
            resolve_section(Some(&settings), Some("python")),
            json!({"analysis": {"exclude": ["**/target"], "extraPaths": []}, "pythonPath": "/usr/bin/python3"})
        );
    }

    #[test]
    fn resolve_section_missing_path_returns_empty_object() {
        let settings = json!({"python": {"analysis": {}}});
        assert_eq!(resolve_section(Some(&settings), Some("rust")), json!({}));
        assert_eq!(
            resolve_section(Some(&settings), Some("python.nonexistent")),
            json!({})
        );
    }

    #[test]
    fn resolve_section_none_settings_returns_empty_object() {
        assert_eq!(resolve_section(None, Some("python")), json!({}));
    }

    #[test]
    fn resolve_section_none_section_returns_empty_object() {
        let settings = json!({"python": {}});
        assert_eq!(resolve_section(Some(&settings), None), json!({}));
    }

    #[test]
    fn configuration_request_uses_settings() {
        let inbox = ServerInbox::new(
            "test".to_string(),
            Some(json!({"mockls": {"key": "value"}})),
        );
        let result = inbox
            .on_request(
                "workspace/configuration",
                &json!({"items": [{"section": "mockls"}]}),
            )
            .expect("configuration request should succeed");
        assert_eq!(result, json!([{"key": "value"}]));
    }

    #[test]
    fn configuration_request_without_settings_returns_empty_objects() {
        let inbox = test_inbox();
        let result = inbox
            .on_request(
                "workspace/configuration",
                &json!({"items": [{"section": "mockls"}, {"section": "other"}]}),
            )
            .expect("configuration request should succeed");
        assert_eq!(result, json!([{}, {}]));
    }

    #[test]
    fn register_capability_accepted() {
        let inbox = test_inbox();
        let result = inbox
            .on_request(
                "client/registerCapability",
                &json!({"registrations": [{"id": "1", "method": "textDocument/didChangeConfiguration"}]}),
            )
            .expect("registerCapability should succeed");
        assert_eq!(result, Value::Null);
    }

    #[test]
    fn unregister_capability_accepted() {
        let inbox = test_inbox();
        let result = inbox
            .on_request(
                "client/unregisterCapability",
                &json!({"unregisterations": [{"id": "1", "method": "textDocument/didChangeConfiguration"}]}),
            )
            .expect("unregisterCapability should succeed");
        assert_eq!(result, Value::Null);
    }

    #[test]
    fn show_message_request_accepted() {
        let inbox = test_inbox();
        let result = inbox
            .on_request(
                "window/showMessageRequest",
                &json!({"type": 1, "message": "Restart?", "actions": [{"title": "Yes"}]}),
            )
            .expect("showMessageRequest should succeed");
        assert_eq!(result, Value::Null);
    }

    #[test]
    fn unknown_request_rejected() {
        let inbox = test_inbox();
        let err = inbox
            .on_request("custom/unknownMethod", &json!({}))
            .expect_err("unknown method should be rejected");
        assert_eq!(err.code, -32601);
    }

    #[test]
    fn is_progress_active_begin_end() {
        let inbox = test_inbox();
        assert!(!inbox.is_progress_active());

        // Progress begin
        inbox.on_notification(
            "$/progress",
            &json!({
                "token": "test-token",
                "value": { "kind": "begin", "title": "Indexing", "percentage": 0 }
            }),
        );
        assert!(inbox.is_progress_active());

        // Progress end
        inbox.on_notification(
            "$/progress",
            &json!({
                "token": "test-token",
                "value": { "kind": "end" }
            }),
        );
        assert!(!inbox.is_progress_active());
    }

    /// Helper that creates a test inbox with an `LspServer` profile attached.
    fn test_inbox_with_server() -> ServerInbox {
        let inbox = test_inbox();
        let server = Arc::new(LspServer::new());
        inbox.set_lsp_server(server);
        inbox
    }

    #[test]
    fn progress_begin_end_updates_server_profile() {
        let inbox = test_inbox_with_server();
        let server = inbox.lsp_server().expect("server should be set");
        assert!(!server.sends_progress());
        assert_eq!(server.in_progress_count(), 0);

        // Begin
        inbox.on_notification(
            "$/progress",
            &json!({
                "token": "tok-1",
                "value": { "kind": "begin", "title": "Checking", "percentage": 0 }
            }),
        );
        assert!(server.sends_progress());
        assert_eq!(server.in_progress_count(), 1);

        // Second begin (overlapping token)
        inbox.on_notification(
            "$/progress",
            &json!({
                "token": "tok-2",
                "value": { "kind": "begin", "title": "Indexing", "percentage": 0 }
            }),
        );
        assert_eq!(server.in_progress_count(), 2);

        // End first
        inbox.on_notification(
            "$/progress",
            &json!({
                "token": "tok-1",
                "value": { "kind": "end" }
            }),
        );
        assert_eq!(server.in_progress_count(), 1);

        // End second
        inbox.on_notification(
            "$/progress",
            &json!({
                "token": "tok-2",
                "value": { "kind": "end" }
            }),
        );
        assert_eq!(server.in_progress_count(), 0);
    }
}

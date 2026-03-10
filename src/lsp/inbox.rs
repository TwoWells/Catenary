// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Shared server state and notification dispatch.

use lsp_types::{ProgressParams, PublishDiagnosticsParams, Uri};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, trace, warn};

use super::client::DiagnosticsCache;
use super::protocol::NotificationMessage;
use super::state::{ProgressTracker, ServerState};
use crate::session::{EventBroadcaster, EventKind};

/// Shared server state for notification dispatch.
///
/// Groups all state that the reader task needs to update when processing
/// LSP notifications. Passed as `Arc<ServerInbox>` to the reader task.
pub struct ServerInbox {
    // Diagnostics
    pub(crate) diagnostics: DiagnosticsCache,
    pub(crate) diagnostics_generation: Arc<Mutex<HashMap<Uri, u64>>>,
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
    pub(crate) has_published_diagnostics: Arc<AtomicBool>,
    pub(crate) publishes_version: Arc<AtomicBool>,
    pub(crate) has_sent_progress: Arc<AtomicBool>,

    // Identity / broadcast
    pub(crate) language: String,
    pub(crate) broadcaster: EventBroadcaster,
}

impl ServerInbox {
    /// Creates a new `ServerInbox` with default state.
    pub(crate) fn new(language: String, broadcaster: EventBroadcaster) -> Self {
        Self {
            diagnostics: Arc::new(Mutex::new(HashMap::new())),
            diagnostics_generation: Arc::new(Mutex::new(HashMap::new())),
            diagnostics_notify: Arc::new(Notify::new()),
            capability_notify: Arc::new(Notify::new()),
            progress: Arc::new(Mutex::new(ProgressTracker::new())),
            progress_notify: Arc::new(Notify::new()),
            state: Arc::new(AtomicU8::new(ServerState::Initializing.as_u8())),
            state_notify: Arc::new(Notify::new()),
            has_published_diagnostics: Arc::new(AtomicBool::new(false)),
            publishes_version: Arc::new(AtomicBool::new(false)),
            has_sent_progress: Arc::new(AtomicBool::new(false)),
            language,
            broadcaster,
        }
    }

    /// Handles an incoming LSP notification, updating shared state.
    pub(crate) async fn on_notification(&self, notification: &NotificationMessage) {
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
                    self.has_published_diagnostics.store(true, Ordering::SeqCst);

                    // Track whether server provides version in diagnostics
                    if params.version.is_some()
                        && !self.publishes_version.swap(true, Ordering::SeqCst)
                    {
                        self.capability_notify.notify_waiters();
                    }

                    let mut cache = self.diagnostics.lock().await;
                    cache.insert(params.uri.clone(), (params.version, params.diagnostics));
                    drop(cache);

                    // Bump generation counter and wake waiters
                    let mut generations = self.diagnostics_generation.lock().await;
                    let counter = generations.entry(params.uri).or_insert(0);
                    *counter += 1;
                    drop(generations);
                    self.diagnostics_notify.notify_waiters();
                } else {
                    warn!("Failed to parse publishDiagnostics params");
                }
            }
            "$/progress" => {
                if let Ok(params) =
                    serde_json::from_value::<ProgressParams>(notification.params.clone())
                {
                    if !self.has_sent_progress.swap(true, Ordering::SeqCst) {
                        self.capability_notify.notify_waiters();
                    }

                    let mut tracker = self.progress.lock().await;
                    tracker.update(&params);

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
                                self.broadcaster.send(EventKind::Progress {
                                    language: self.language.clone(),
                                    title: p.title.clone(),
                                    message: p.message.clone(),
                                    percentage: p.percentage,
                                });
                            }
                        } else {
                            self.state
                                .store(ServerState::Ready.as_u8(), Ordering::SeqCst);
                            debug!("Server ready (progress completed)");
                            self.broadcaster.send(EventKind::ProgressEnd {
                                language: self.language.clone(),
                            });
                        }
                        // Fire notifies after state update
                        self.progress_notify.notify_waiters();
                        self.state_notify.notify_waiters();
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
}

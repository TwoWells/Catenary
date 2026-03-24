// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Shared container for tool servers and cross-tool infrastructure.
//!
//! Owns the tool implementations and the dependencies they share.
//! `LspBridgeHandler` holds a `Toolbox` and handles protocol boundary
//! concerns (health checks, readiness, dispatch routing).

use std::collections::HashSet;
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::Mutex;

use super::DocumentManager;
use super::diagnostics_server::DiagnosticsServer;
use super::editing::EditingServer;
use super::file_tools::GlobServer;
use super::filesystem_manager::FilesystemManager;
use super::grep_server::GrepServer;
use crate::lsp::ClientManager;

/// Shared container for tool servers and cross-tool infrastructure.
///
/// Owns the tool implementations and the dependencies they share.
/// [`super::handler::LspBridgeHandler`] holds a `Toolbox` and handles protocol boundary
/// concerns (health checks, readiness, dispatch routing).
pub struct Toolbox {
    /// Grep tool server.
    pub grep: GrepServer,
    /// Glob tool server.
    pub glob: GlobServer,
    /// Per-file diagnostic batching (`start_editing` / `done_editing`).
    pub editing: EditingServer,
    /// Shared LSP client manager.
    pub client_manager: Arc<ClientManager>,
    /// Shared document manager.
    pub doc_manager: Arc<Mutex<DocumentManager>>,
    /// Tokio runtime handle for blocking dispatch.
    pub runtime: Handle,
    /// Cross-tool filesystem classification (binary detection, language ID).
    pub fs_manager: Arc<FilesystemManager>,
}

impl Toolbox {
    /// Creates a new `Toolbox` with all tool servers and shared dependencies.
    pub fn new(
        client_manager: Arc<ClientManager>,
        doc_manager: Arc<Mutex<DocumentManager>>,
        runtime: Handle,
        diagnostics: Arc<DiagnosticsServer>,
        session_id: Option<String>,
    ) -> Self {
        let fs_manager = Arc::new(FilesystemManager::new());
        let notified_offline = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let editing = EditingServer::new(diagnostics, session_id.unwrap_or_default());
        let grep = GrepServer {
            client_manager: client_manager.clone(),
            doc_manager: doc_manager.clone(),
            fs_manager: fs_manager.clone(),
            notified_offline: notified_offline.clone(),
        };
        let glob = GlobServer {
            client_manager: client_manager.clone(),
            doc_manager: doc_manager.clone(),
            fs_manager: fs_manager.clone(),
            notified_offline,
        };
        Self {
            grep,
            glob,
            editing,
            client_manager,
            doc_manager,
            runtime,
            fs_manager,
        }
    }
}

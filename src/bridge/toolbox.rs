// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Shared application container for tool servers and cross-tool infrastructure.
//!
//! `Toolbox` creates and owns all internal servers and shared dependencies.
//! Protocol boundaries (`LspBridgeHandler`, `HookServer`) hold `Arc<Toolbox>`
//! and access any dependency through it.

use anyhow::Result;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::RwLock;

use super::diagnostics_server::DiagnosticsServer;
use super::file_tools::GlobServer;
use super::filesystem_manager::FilesystemManager;
use super::grep_server::GrepServer;
use super::path_security::PathValidator;
use crate::config::Config;
use crate::lsp::LspClientManager;
use crate::session::MessageLog;

/// Shared application container for tool servers and cross-tool infrastructure.
///
/// Creates and owns all internal servers and shared dependencies.
/// [`super::handler::LspBridgeHandler`] holds an `Arc<Toolbox>` and handles
/// protocol boundary concerns (health checks, readiness, dispatch routing).
pub struct Toolbox {
    /// Grep tool server.
    pub grep: GrepServer,
    /// Glob tool server.
    pub glob: GlobServer,
    /// Diagnostics pipeline for `PostToolUse` hook requests.
    pub diagnostics: Arc<DiagnosticsServer>,
    /// LSP client manager (also owns document manager).
    pub(super) client_manager: Arc<LspClientManager>,
    /// File classification and root resolution.
    fs_manager: Arc<FilesystemManager>,
    /// Path validation for LSP-aware operations.
    path_validator: Arc<RwLock<PathValidator>>,
    /// Tokio runtime handle for blocking dispatch.
    pub runtime: Handle,
}

impl Toolbox {
    /// Creates a new `Toolbox`, constructing all internal dependencies.
    #[must_use]
    pub fn new(
        config: Config,
        roots: Vec<PathBuf>,
        message_log: Arc<MessageLog>,
        session_id: String,
        runtime: Handle,
    ) -> Self {
        let fs_manager = Arc::new(FilesystemManager::new());
        fs_manager.set_roots(roots.clone());
        let path_validator = Arc::new(RwLock::new(PathValidator::new(roots.clone())));
        let client_manager = Arc::new(LspClientManager::new(
            config,
            roots,
            message_log,
            fs_manager.clone(),
            session_id,
        ));
        let diagnostics = Arc::new(DiagnosticsServer::new(
            client_manager.clone(),
            path_validator.clone(),
        ));
        let notified_offline = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let grep = GrepServer {
            client_manager: client_manager.clone(),
            fs_manager: fs_manager.clone(),
            notified_offline: notified_offline.clone(),
        };
        let glob = GlobServer {
            client_manager: client_manager.clone(),
            fs_manager: fs_manager.clone(),
            notified_offline,
        };
        Self {
            grep,
            glob,
            diagnostics,
            client_manager,
            fs_manager,
            path_validator,
            runtime,
        }
    }

    /// Spawns LSP servers for languages detected in the workspace.
    pub async fn spawn_all(&self) {
        self.client_manager.spawn_all().await;
    }

    /// Synchronizes workspace roots with a new set.
    ///
    /// Updates path validation, notifies LSP servers of folder changes,
    /// and spawns servers for any newly detected languages.
    ///
    /// # Errors
    ///
    /// Returns an error if root synchronization fails.
    pub async fn sync_roots(&self, roots: Vec<PathBuf>) -> Result<()> {
        self.fs_manager.set_roots(roots.clone());
        self.path_validator
            .write()
            .await
            .update_roots(roots.clone());
        self.client_manager.sync_roots(roots).await?;

        // Fire-and-forget: spawn_all is pre-warming, not a gate.
        // Tool calls that need a server will trigger get_client on demand.
        let cm = self.client_manager.clone();
        tokio::spawn(async move { cm.spawn_all().await });
        Ok(())
    }

    /// Shuts down all active LSP servers gracefully.
    pub async fn shutdown(&self) {
        self.client_manager.shutdown_all().await;
    }
}

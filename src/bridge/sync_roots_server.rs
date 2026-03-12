// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Workspace root synchronization for PreToolUse hook requests.
//!
//! Handles full root replacement (`sync_roots`) and incremental additions
//! (`add_roots`) by canonicalizing paths, diffing against the current root
//! set, updating the path validator, and notifying LSP clients.

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use super::path_security::PathValidator;
use crate::lsp::ClientManager;

/// Handles `PreToolUse` hook requests: workspace root synchronization.
pub struct SyncRootsServer {
    client_manager: Arc<ClientManager>,
    path_validator: Arc<RwLock<PathValidator>>,
}

impl SyncRootsServer {
    /// Creates a new `SyncRootsServer`.
    pub const fn new(
        client_manager: Arc<ClientManager>,
        path_validator: Arc<RwLock<PathValidator>>,
    ) -> Self {
        Self {
            client_manager,
            path_validator,
        }
    }

    /// Synchronizes the full workspace root set.
    ///
    /// Canonicalizes incoming paths, diffs against the current root set, and
    /// applies both additions and removals. Uses `ClientManager::sync_roots()`
    /// to send a single `didChangeWorkspaceFolders` notification per LSP client.
    pub async fn sync_roots(&self, paths: &[String]) -> String {
        match self.sync_roots_inner(paths).await {
            Ok(msg) => msg,
            Err(e) => format!("Notify error: {e}"),
        }
    }

    /// Inner implementation for `sync_roots`.
    async fn sync_roots_inner(&self, paths: &[String]) -> Result<String> {
        let mut new_roots = Vec::new();
        for p in paths {
            let path = PathBuf::from(p);
            match path.canonicalize() {
                Ok(canonical) => {
                    if !new_roots.contains(&canonical) {
                        new_roots.push(canonical);
                    }
                }
                Err(e) => {
                    debug!("Skipping root {p}: {e}");
                }
            }
        }

        let current_roots = self.path_validator.read().await.roots().to_vec();

        // Check if anything actually changed
        if new_roots == current_roots {
            return Ok(String::new());
        }

        let added: Vec<String> = new_roots
            .iter()
            .filter(|r| !current_roots.contains(r))
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let removed: Vec<String> = current_roots
            .iter()
            .filter(|r| !new_roots.contains(r))
            .map(|p| p.to_string_lossy().into_owned())
            .collect();

        if added.is_empty() && removed.is_empty() {
            return Ok(String::new());
        }

        // Update path validator
        self.path_validator
            .write()
            .await
            .update_roots(new_roots.clone());

        // Sync LSP clients (handles both additions and removals)
        if let Err(e) = self.client_manager.sync_roots(new_roots).await {
            warn!("Failed to sync roots with LSP clients: {e}");
        }

        // Spawn any new language servers for added roots
        if !added.is_empty() {
            self.client_manager.spawn_all().await;
        }

        let mut parts = Vec::new();
        if !added.is_empty() {
            info!("Added roots: {}", added.join(", "));
            parts.push(format!("Added roots: {}", added.join(", ")));
        }
        if !removed.is_empty() {
            info!("Removed roots: {}", removed.join(", "));
            parts.push(format!("Removed roots: {}", removed.join(", ")));
        }
        Ok(parts.join("\n"))
    }

    /// Adds new workspace roots incrementally.
    ///
    /// Canonicalizes each path, filters to genuinely new roots, updates the
    /// path validator, notifies LSP clients, and spawns servers for new languages.
    pub async fn add_roots(&self, paths: &[String]) -> String {
        match self.add_roots_inner(paths).await {
            Ok(msg) => msg,
            Err(e) => format!("Notify error: {e}"),
        }
    }

    /// Inner implementation for `add_roots`.
    async fn add_roots_inner(&self, paths: &[String]) -> Result<String> {
        // Canonicalize each path, skipping any that don't exist
        let mut new_paths = Vec::new();
        for p in paths {
            let path = PathBuf::from(p);
            match path.canonicalize() {
                Ok(canonical) => new_paths.push(canonical),
                Err(e) => {
                    debug!("Skipping root {p}: {e}");
                }
            }
        }

        if new_paths.is_empty() {
            return Ok(String::new());
        }

        // Get current roots and filter to only genuinely new ones
        let current_roots = self.path_validator.read().await.roots().to_vec();
        let genuinely_new: Vec<PathBuf> = new_paths
            .into_iter()
            .filter(|p| !current_roots.contains(p))
            .collect();

        if genuinely_new.is_empty() {
            return Ok(String::new());
        }

        // Build new full root list
        let mut all_roots = current_roots;
        all_roots.extend(genuinely_new.iter().cloned());

        // Update path validator
        self.path_validator.write().await.update_roots(all_roots);

        // Notify LSP clients about each new root
        for root in &genuinely_new {
            if let Err(e) = self.client_manager.add_root(root.clone()).await {
                warn!("Failed to add root {}: {e}", root.display());
            }
        }

        // Spawn any new language servers for the added roots
        self.client_manager.spawn_all().await;

        let added: Vec<String> = genuinely_new
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        info!("Added roots: {}", added.join(", "));
        Ok(format!("Added roots: {}", added.join(", ")))
    }
}

// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Server profile: what Catenary learned from the init handshake and
//! observes at runtime.
//!
//! `LspServer` is the single source of truth for server behavior.
//! Init-time fields are immutable; runtime fields use interior mutability
//! (`OnceLock`, `AtomicU32`) for lock-free reads from any thread.

use serde_json::Value;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use super::extract;

/// Server profile capturing init-time capabilities and runtime observations.
///
/// Shared via `Arc<LspServer>` between [`super::LspClient`] and
/// `ServerInbox`. All runtime fields use interior mutability
/// so readers never need a lock.
pub struct LspServer {
    /// Raw server capabilities from the `initialize` response.
    capabilities: Value,

    /// Whether the server advertises `diagnosticProvider` (pull model).
    pulls_diagnostics: bool,

    /// Set on first `textDocument/publishDiagnostics` notification.
    pushes_diagnostics: OnceLock<()>,

    /// Set on first `$/progress` begin.
    sends_progress: OnceLock<()>,

    /// Count of in-flight progress tokens (begin increments, end decrements).
    in_progress_count: AtomicU32,
}

impl LspServer {
    /// Creates a new server profile from the capabilities extracted during
    /// the `initialize` handshake.
    #[must_use]
    pub fn new(capabilities: Value) -> Self {
        let pulls_diagnostics = extract::has_diagnostic_provider(&capabilities);
        Self {
            capabilities,
            pulls_diagnostics,
            pushes_diagnostics: OnceLock::new(),
            sends_progress: OnceLock::new(),
            in_progress_count: AtomicU32::new(0),
        }
    }

    /// Returns the raw server capabilities.
    pub const fn capabilities(&self) -> &Value {
        &self.capabilities
    }

    /// Returns whether a capability key is present and non-null.
    ///
    /// LSP capabilities can be `true`, `{}`, or a detailed options object —
    /// all are truthy. Only absent or `null` means unsupported.
    fn has_capability(&self, key: &str) -> bool {
        self.capabilities.get(key).is_some_and(|v| !v.is_null())
    }

    /// Returns whether the server advertises `diagnosticProvider` (pull model).
    pub const fn pulls_diagnostics(&self) -> bool {
        self.pulls_diagnostics
    }

    /// Returns whether the server advertises `hoverProvider`.
    pub fn supports_hover(&self) -> bool {
        self.has_capability("hoverProvider")
    }

    /// Returns whether the server advertises `definitionProvider`.
    pub fn supports_definition(&self) -> bool {
        self.has_capability("definitionProvider")
    }

    /// Returns whether the server advertises `referencesProvider`.
    pub fn supports_references(&self) -> bool {
        self.has_capability("referencesProvider")
    }

    /// Returns whether the server advertises `documentSymbolProvider`.
    pub fn supports_document_symbols(&self) -> bool {
        self.has_capability("documentSymbolProvider")
    }

    /// Returns whether the server advertises `workspaceSymbolProvider`.
    pub fn supports_workspace_symbols(&self) -> bool {
        self.has_capability("workspaceSymbolProvider")
    }

    /// Returns whether the server advertises `renameProvider`.
    pub fn supports_rename(&self) -> bool {
        self.has_capability("renameProvider")
    }

    /// Returns whether the server advertises `typeDefinitionProvider`.
    pub fn supports_type_definition(&self) -> bool {
        self.has_capability("typeDefinitionProvider")
    }

    /// Returns whether the server advertises `implementationProvider`.
    pub fn supports_implementation(&self) -> bool {
        self.has_capability("implementationProvider")
    }

    /// Returns whether the server advertises `callHierarchyProvider`.
    pub fn supports_call_hierarchy(&self) -> bool {
        self.has_capability("callHierarchyProvider")
    }

    /// Returns whether the server advertises `typeHierarchyProvider`.
    pub fn supports_type_hierarchy(&self) -> bool {
        self.has_capability("typeHierarchyProvider")
    }

    /// Returns whether the server advertises `codeActionProvider`.
    pub fn supports_code_action(&self) -> bool {
        self.has_capability("codeActionProvider")
    }

    /// Returns whether the server has ever sent `textDocument/publishDiagnostics`.
    pub fn pushes_diagnostics(&self) -> bool {
        self.pushes_diagnostics.get().is_some()
    }

    /// Returns whether the server has ever sent a `$/progress` begin.
    pub fn sends_progress(&self) -> bool {
        self.sends_progress.get().is_some()
    }

    /// Returns the number of in-flight progress tokens.
    pub fn in_progress_count(&self) -> u32 {
        self.in_progress_count.load(Ordering::SeqCst)
    }

    /// Records a `$/progress` begin: sets `sends_progress` (once) and
    /// increments the in-flight count.
    ///
    /// Returns `true` if this was the first progress begin (capability
    /// discovery moment).
    pub fn on_progress_begin(&self) -> bool {
        let first = self.sends_progress.set(()).is_ok();
        self.in_progress_count.fetch_add(1, Ordering::SeqCst);
        first
    }

    /// Records a `$/progress` end: decrements the in-flight count
    /// (saturating at zero).
    pub fn on_progress_end(&self) {
        self.in_progress_count
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                Some(n.saturating_sub(1))
            })
            .ok();
    }

    /// Records the first `textDocument/publishDiagnostics` notification.
    pub fn on_publish_diagnostics(&self) {
        let _ = self.pushes_diagnostics.set(());
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

    #[test]
    fn new_extracts_pulls_diagnostics() {
        let caps = json!({ "diagnosticProvider": { "interFileDependencies": true } });
        let server = LspServer::new(caps);
        assert!(server.pulls_diagnostics());
    }

    #[test]
    fn new_no_diagnostic_provider() {
        let server = LspServer::new(json!({}));
        assert!(!server.pulls_diagnostics());
    }

    #[test]
    fn on_publish_diagnostics_sets_flag() {
        let server = LspServer::new(json!({}));
        assert!(!server.pushes_diagnostics());

        server.on_publish_diagnostics();
        assert!(server.pushes_diagnostics());

        // Idempotent
        server.on_publish_diagnostics();
        assert!(server.pushes_diagnostics());
    }

    #[test]
    fn on_progress_begin_end_count() {
        let server = LspServer::new(json!({}));
        assert_eq!(server.in_progress_count(), 0);

        server.on_progress_begin();
        server.on_progress_begin();
        assert_eq!(server.in_progress_count(), 2);

        server.on_progress_end();
        assert_eq!(server.in_progress_count(), 1);

        server.on_progress_end();
        assert_eq!(server.in_progress_count(), 0);
    }

    #[test]
    fn on_progress_begin_sets_sends_progress() {
        let server = LspServer::new(json!({}));
        assert!(!server.sends_progress());

        server.on_progress_begin();
        assert!(server.sends_progress());
    }

    #[test]
    fn in_progress_count_saturates() {
        let server = LspServer::new(json!({}));
        assert_eq!(server.in_progress_count(), 0);

        server.on_progress_end();
        assert_eq!(server.in_progress_count(), 0);

        // Multiple underflow attempts stay at zero
        server.on_progress_end();
        server.on_progress_end();
        assert_eq!(server.in_progress_count(), 0);
    }

    // ── Capability checks ──────────────────────────────────────────

    #[test]
    fn supports_capability_true() {
        let server = LspServer::new(json!({ "workspaceSymbolProvider": true }));
        assert!(server.supports_workspace_symbols());
    }

    #[test]
    fn supports_capability_options_object() {
        let server = LspServer::new(json!({ "workspaceSymbolProvider": {} }));
        assert!(server.supports_workspace_symbols());
    }

    #[test]
    fn supports_capability_detailed_options() {
        let server = LspServer::new(json!({
            "workspaceSymbolProvider": { "resolveProvider": true }
        }));
        assert!(server.supports_workspace_symbols());
    }

    #[test]
    fn supports_capability_missing() {
        let server = LspServer::new(json!({}));
        assert!(!server.supports_workspace_symbols());
    }

    #[test]
    fn supports_capability_null() {
        let server = LspServer::new(json!({ "workspaceSymbolProvider": null }));
        assert!(!server.supports_workspace_symbols());
    }

    #[test]
    fn empty_capabilities_nothing_supported() {
        let server = LspServer::new(json!({}));
        assert!(!server.supports_hover());
        assert!(!server.supports_definition());
        assert!(!server.supports_references());
        assert!(!server.supports_document_symbols());
        assert!(!server.supports_workspace_symbols());
        assert!(!server.supports_rename());
        assert!(!server.supports_type_definition());
        assert!(!server.supports_implementation());
        assert!(!server.supports_call_hierarchy());
        assert!(!server.supports_type_hierarchy());
        assert!(!server.supports_code_action());
    }

    #[test]
    fn supports_all_capabilities() {
        let server = LspServer::new(json!({
            "hoverProvider": true,
            "definitionProvider": true,
            "referencesProvider": true,
            "documentSymbolProvider": true,
            "workspaceSymbolProvider": true,
            "renameProvider": true,
            "typeDefinitionProvider": true,
            "implementationProvider": true,
            "callHierarchyProvider": true,
            "typeHierarchyProvider": true,
            "codeActionProvider": true,
        }));
        assert!(server.supports_hover());
        assert!(server.supports_definition());
        assert!(server.supports_references());
        assert!(server.supports_document_symbols());
        assert!(server.supports_workspace_symbols());
        assert!(server.supports_rename());
        assert!(server.supports_type_definition());
        assert!(server.supports_implementation());
        assert!(server.supports_call_hierarchy());
        assert!(server.supports_type_hierarchy());
        assert!(server.supports_code_action());
    }
}

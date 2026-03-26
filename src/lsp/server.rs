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

/// Server profile capturing init-time capabilities and runtime observations.
///
/// Shared via `Arc<LspServer>` between [`super::LspClient`] and
/// `ServerInbox`. All runtime fields use interior mutability
/// so readers never need a lock.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent capability flags from LSP init"
)]
pub struct LspServer {
    /// Raw server capabilities from the `initialize` response.
    capabilities: Value,

    // ── Init-time capability flags (immutable after construction) ───
    supports_pull_diagnostics: bool,
    supports_hover: bool,
    supports_definition: bool,
    supports_references: bool,
    supports_document_symbols: bool,
    supports_workspace_symbols: bool,
    supports_workspace_symbol_resolve: bool,
    supports_rename: bool,
    supports_type_definition: bool,
    supports_implementation: bool,
    supports_call_hierarchy: bool,
    supports_type_hierarchy: bool,
    supports_code_action: bool,

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
        // LSP capabilities are `boolean | Options`. `true` or an options
        // object means supported; `false`, `null`, or absent means not.
        let has = |key: &str| {
            capabilities
                .get(key)
                .is_some_and(|v| v.as_bool() != Some(false) && !v.is_null())
        };
        Self {
            supports_pull_diagnostics: has("diagnosticProvider"),
            supports_hover: has("hoverProvider"),
            supports_definition: has("definitionProvider"),
            supports_references: has("referencesProvider"),
            supports_document_symbols: has("documentSymbolProvider"),
            supports_workspace_symbols: has("workspaceSymbolProvider"),
            supports_workspace_symbol_resolve: capabilities
                .get("workspaceSymbolProvider")
                .and_then(|v| v.get("resolveProvider"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            supports_rename: has("renameProvider"),
            supports_type_definition: has("typeDefinitionProvider"),
            supports_implementation: has("implementationProvider"),
            supports_call_hierarchy: has("callHierarchyProvider"),
            supports_type_hierarchy: has("typeHierarchyProvider"),
            supports_code_action: has("codeActionProvider"),
            capabilities,
            pushes_diagnostics: OnceLock::new(),
            sends_progress: OnceLock::new(),
            in_progress_count: AtomicU32::new(0),
        }
    }

    /// Returns the raw server capabilities.
    pub const fn capabilities(&self) -> &Value {
        &self.capabilities
    }

    /// Returns whether the server advertises `diagnosticProvider` (pull model).
    pub const fn supports_pull_diagnostics(&self) -> bool {
        self.supports_pull_diagnostics
    }

    /// Returns whether the server advertises `hoverProvider`.
    pub const fn supports_hover(&self) -> bool {
        self.supports_hover
    }

    /// Returns whether the server advertises `definitionProvider`.
    pub const fn supports_definition(&self) -> bool {
        self.supports_definition
    }

    /// Returns whether the server advertises `referencesProvider`.
    pub const fn supports_references(&self) -> bool {
        self.supports_references
    }

    /// Returns whether the server advertises `documentSymbolProvider`.
    pub const fn supports_document_symbols(&self) -> bool {
        self.supports_document_symbols
    }

    /// Returns whether the server advertises `workspaceSymbolProvider`.
    pub const fn supports_workspace_symbols(&self) -> bool {
        self.supports_workspace_symbols
    }

    /// Returns whether the server advertises `workspaceSymbolProvider.resolveProvider`.
    pub const fn supports_workspace_symbol_resolve(&self) -> bool {
        self.supports_workspace_symbol_resolve
    }

    /// Returns whether the server advertises `renameProvider`.
    pub const fn supports_rename(&self) -> bool {
        self.supports_rename
    }

    /// Returns whether the server advertises `typeDefinitionProvider`.
    pub const fn supports_type_definition(&self) -> bool {
        self.supports_type_definition
    }

    /// Returns whether the server advertises `implementationProvider`.
    pub const fn supports_implementation(&self) -> bool {
        self.supports_implementation
    }

    /// Returns whether the server advertises `callHierarchyProvider`.
    pub const fn supports_call_hierarchy(&self) -> bool {
        self.supports_call_hierarchy
    }

    /// Returns whether the server advertises `typeHierarchyProvider`.
    pub const fn supports_type_hierarchy(&self) -> bool {
        self.supports_type_hierarchy
    }

    /// Returns whether the server advertises `codeActionProvider`.
    pub const fn supports_code_action(&self) -> bool {
        self.supports_code_action
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
    fn new_extracts_supports_pull_diagnostics() {
        let caps = json!({ "diagnosticProvider": { "interFileDependencies": true } });
        let server = LspServer::new(caps);
        assert!(server.supports_pull_diagnostics());
    }

    #[test]
    fn new_no_diagnostic_provider() {
        let server = LspServer::new(json!({}));
        assert!(!server.supports_pull_diagnostics());
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
    fn supports_capability_false() {
        let server = LspServer::new(json!({ "workspaceSymbolProvider": false }));
        assert!(!server.supports_workspace_symbols());
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
    fn explicit_false_not_supported() {
        let server = LspServer::new(json!({
            "hoverProvider": false,
            "definitionProvider": false,
            "referencesProvider": false,
            "documentSymbolProvider": false,
            "workspaceSymbolProvider": false,
            "renameProvider": false,
            "typeDefinitionProvider": false,
            "implementationProvider": false,
            "callHierarchyProvider": false,
            "typeHierarchyProvider": false,
            "codeActionProvider": false,
        }));
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
    fn empty_capabilities_nothing_supported() {
        let server = LspServer::new(json!({}));
        assert!(!server.supports_hover());
        assert!(!server.supports_definition());
        assert!(!server.supports_references());
        assert!(!server.supports_document_symbols());
        assert!(!server.supports_workspace_symbols());
        assert!(!server.supports_workspace_symbol_resolve());
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
            "workspaceSymbolProvider": { "resolveProvider": true },
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
        assert!(server.supports_workspace_symbol_resolve());
        assert!(server.supports_rename());
        assert!(server.supports_type_definition());
        assert!(server.supports_implementation());
        assert!(server.supports_call_hierarchy());
        assert!(server.supports_type_hierarchy());
        assert!(server.supports_code_action());
    }

    // ── Workspace symbol resolve ───────────────────────────────────

    #[test]
    fn workspace_symbol_resolve_boolean_provider() {
        // workspaceSymbolProvider: true — no resolveProvider field
        let server = LspServer::new(json!({ "workspaceSymbolProvider": true }));
        assert!(server.supports_workspace_symbols());
        assert!(!server.supports_workspace_symbol_resolve());
    }

    #[test]
    fn workspace_symbol_resolve_empty_options() {
        // workspaceSymbolProvider: {} — supported but no resolve
        let server = LspServer::new(json!({ "workspaceSymbolProvider": {} }));
        assert!(server.supports_workspace_symbols());
        assert!(!server.supports_workspace_symbol_resolve());
    }

    #[test]
    fn workspace_symbol_resolve_false() {
        let server = LspServer::new(json!({
            "workspaceSymbolProvider": { "resolveProvider": false }
        }));
        assert!(server.supports_workspace_symbols());
        assert!(!server.supports_workspace_symbol_resolve());
    }

    #[test]
    fn workspace_symbol_resolve_true() {
        let server = LspServer::new(json!({
            "workspaceSymbolProvider": { "resolveProvider": true }
        }));
        assert!(server.supports_workspace_symbols());
        assert!(server.supports_workspace_symbol_resolve());
    }
}

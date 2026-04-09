// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Server profile: what Catenary learned from the init handshake and
//! observes at runtime.
//!
//! `LspServer` is created at spawn time (before `initialize`) and is
//! the single source of truth for server behavior. Capabilities are
//! set once via [`LspServer::set_capabilities`] after the init handshake.
//! All fields use interior mutability (`OnceLock`, `AtomicU32`) for
//! lock-free reads from any thread.

use serde_json::Value;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

/// Server profile capturing init-time capabilities and runtime observations.
///
/// Created at spawn time with empty `OnceLock` fields. Capabilities are
/// populated once via [`Self::set_capabilities`] after the `initialize`
/// handshake completes. Shared via `Arc<LspServer>` between
/// [`super::LspClient`] and `ServerInbox`. All runtime fields use
/// interior mutability so readers never need a lock.
pub struct LspServer {
    /// Raw server capabilities from the `initialize` response.
    /// Set once via [`Self::set_capabilities`].
    capabilities: OnceLock<Value>,

    // ── Init-time capability flags (set once via set_capabilities) ──
    capability_flags: OnceLock<CapabilityFlags>,

    /// Set on first `$/progress` begin.
    sends_progress: OnceLock<()>,

    /// Count of in-flight progress tokens (begin increments, end decrements).
    in_progress_count: AtomicU32,
}

/// Capability flags extracted from the `initialize` response.
///
/// All fields are immutable after construction.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent capability flags from LSP init"
)]
struct CapabilityFlags {
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
}

impl CapabilityFlags {
    /// Extracts capability flags from the raw `initialize` response capabilities.
    fn from_capabilities(capabilities: &Value) -> Self {
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
        }
    }
}

/// Default flags: nothing supported.
const NO_CAPABILITIES: CapabilityFlags = CapabilityFlags {
    supports_pull_diagnostics: false,
    supports_hover: false,
    supports_definition: false,
    supports_references: false,
    supports_document_symbols: false,
    supports_workspace_symbols: false,
    supports_workspace_symbol_resolve: false,
    supports_rename: false,
    supports_type_definition: false,
    supports_implementation: false,
    supports_call_hierarchy: false,
    supports_type_hierarchy: false,
    supports_code_action: false,
};

impl Default for LspServer {
    fn default() -> Self {
        Self::new()
    }
}

impl LspServer {
    /// Creates a new server profile with no capabilities.
    ///
    /// Call [`Self::set_capabilities`] after the `initialize` handshake
    /// to populate capability fields.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            capabilities: OnceLock::new(),
            capability_flags: OnceLock::new(),
            sends_progress: OnceLock::new(),
            in_progress_count: AtomicU32::new(0),
        }
    }

    /// Sets capabilities from the `initialize` response. Called once.
    ///
    /// Extracts all capability flags and stores the raw capabilities.
    /// Subsequent calls are no-ops (the `OnceLock` ignores them).
    pub fn set_capabilities(&self, capabilities: Value) {
        let flags = CapabilityFlags::from_capabilities(&capabilities);
        let _ = self.capabilities.set(capabilities);
        let _ = self.capability_flags.set(flags);
    }

    /// Returns the raw server capabilities.
    ///
    /// Returns an empty object before [`Self::set_capabilities`] is called.
    pub fn capabilities(&self) -> &Value {
        static EMPTY: OnceLock<Value> = OnceLock::new();
        self.capabilities
            .get()
            .unwrap_or_else(|| EMPTY.get_or_init(|| Value::Object(serde_json::Map::new())))
    }

    /// Returns the capability flags, defaulting to none before init.
    fn flags(&self) -> &CapabilityFlags {
        self.capability_flags.get().unwrap_or(&NO_CAPABILITIES)
    }

    /// Returns whether the server advertises `diagnosticProvider` (pull model).
    pub fn supports_pull_diagnostics(&self) -> bool {
        self.flags().supports_pull_diagnostics
    }

    /// Returns whether the server advertises `hoverProvider`.
    pub fn supports_hover(&self) -> bool {
        self.flags().supports_hover
    }

    /// Returns whether the server advertises `definitionProvider`.
    pub fn supports_definition(&self) -> bool {
        self.flags().supports_definition
    }

    /// Returns whether the server advertises `referencesProvider`.
    pub fn supports_references(&self) -> bool {
        self.flags().supports_references
    }

    /// Returns whether the server advertises `documentSymbolProvider`.
    pub fn supports_document_symbols(&self) -> bool {
        self.flags().supports_document_symbols
    }

    /// Returns whether the server advertises `workspaceSymbolProvider`.
    pub fn supports_workspace_symbols(&self) -> bool {
        self.flags().supports_workspace_symbols
    }

    /// Returns whether the server advertises `workspaceSymbolProvider.resolveProvider`.
    pub fn supports_workspace_symbol_resolve(&self) -> bool {
        self.flags().supports_workspace_symbol_resolve
    }

    /// Returns whether the server advertises `renameProvider`.
    pub fn supports_rename(&self) -> bool {
        self.flags().supports_rename
    }

    /// Returns whether the server advertises `typeDefinitionProvider`.
    pub fn supports_type_definition(&self) -> bool {
        self.flags().supports_type_definition
    }

    /// Returns whether the server advertises `implementationProvider`.
    pub fn supports_implementation(&self) -> bool {
        self.flags().supports_implementation
    }

    /// Returns whether the server advertises `callHierarchyProvider`.
    pub fn supports_call_hierarchy(&self) -> bool {
        self.flags().supports_call_hierarchy
    }

    /// Returns whether the server advertises `typeHierarchyProvider`.
    pub fn supports_type_hierarchy(&self) -> bool {
        self.flags().supports_type_hierarchy
    }

    /// Returns whether the server advertises `codeActionProvider`.
    pub fn supports_code_action(&self) -> bool {
        self.flags().supports_code_action
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
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Helper: creates an `LspServer` with capabilities already set.
    fn server_with_caps(caps: Value) -> LspServer {
        let server = LspServer::new();
        server.set_capabilities(caps);
        server
    }

    #[test]
    fn set_capabilities_extracts_pull_diagnostics() {
        let server =
            server_with_caps(json!({ "diagnosticProvider": { "interFileDependencies": true } }));
        assert!(server.supports_pull_diagnostics());
    }

    #[test]
    fn no_diagnostic_provider() {
        let server = server_with_caps(json!({}));
        assert!(!server.supports_pull_diagnostics());
    }

    #[test]
    fn before_set_capabilities_nothing_supported() {
        let server = LspServer::new();
        assert!(!server.supports_pull_diagnostics());
        assert!(!server.supports_hover());
        assert!(!server.supports_workspace_symbols());
        // capabilities() returns empty object
        assert_eq!(server.capabilities(), &json!({}));
    }

    #[test]
    fn on_progress_begin_end_count() {
        let server = LspServer::new();
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
        let server = LspServer::new();
        assert!(!server.sends_progress());

        server.on_progress_begin();
        assert!(server.sends_progress());
    }

    #[test]
    fn in_progress_count_saturates() {
        let server = LspServer::new();
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
        let server = server_with_caps(json!({ "workspaceSymbolProvider": true }));
        assert!(server.supports_workspace_symbols());
    }

    #[test]
    fn supports_capability_false() {
        let server = server_with_caps(json!({ "workspaceSymbolProvider": false }));
        assert!(!server.supports_workspace_symbols());
    }

    #[test]
    fn supports_capability_options_object() {
        let server = server_with_caps(json!({ "workspaceSymbolProvider": {} }));
        assert!(server.supports_workspace_symbols());
    }

    #[test]
    fn supports_capability_detailed_options() {
        let server = server_with_caps(json!({
            "workspaceSymbolProvider": { "resolveProvider": true }
        }));
        assert!(server.supports_workspace_symbols());
    }

    #[test]
    fn supports_capability_missing() {
        let server = server_with_caps(json!({}));
        assert!(!server.supports_workspace_symbols());
    }

    #[test]
    fn supports_capability_null() {
        let server = server_with_caps(json!({ "workspaceSymbolProvider": null }));
        assert!(!server.supports_workspace_symbols());
    }

    #[test]
    fn explicit_false_not_supported() {
        let server = server_with_caps(json!({
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
        let server = server_with_caps(json!({}));
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
        let server = server_with_caps(json!({
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
        let server = server_with_caps(json!({ "workspaceSymbolProvider": true }));
        assert!(server.supports_workspace_symbols());
        assert!(!server.supports_workspace_symbol_resolve());
    }

    #[test]
    fn workspace_symbol_resolve_empty_options() {
        // workspaceSymbolProvider: {} — supported but no resolve
        let server = server_with_caps(json!({ "workspaceSymbolProvider": {} }));
        assert!(server.supports_workspace_symbols());
        assert!(!server.supports_workspace_symbol_resolve());
    }

    #[test]
    fn workspace_symbol_resolve_false() {
        let server = server_with_caps(json!({
            "workspaceSymbolProvider": { "resolveProvider": false }
        }));
        assert!(server.supports_workspace_symbols());
        assert!(!server.supports_workspace_symbol_resolve());
    }

    #[test]
    fn workspace_symbol_resolve_true() {
        let server = server_with_caps(json!({
            "workspaceSymbolProvider": { "resolveProvider": true }
        }));
        assert!(server.supports_workspace_symbols());
        assert!(server.supports_workspace_symbol_resolve());
    }
}

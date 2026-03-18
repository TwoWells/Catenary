// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

/// Diagnostics pipeline for PostToolUse hook requests.
pub mod diagnostics_server;
/// Manages document lifecycle and sync between disk and LSP servers.
mod document_manager;
/// Glob tool handler: unified file/directory/pattern browsing.
mod file_tools;
/// Grep tool: ripgrep + workspace/symbol pipeline with LSP enrichment.
mod grep_server;
/// Maps MCP tool calls to LSP requests.
mod handler;
/// Path validation for LSP-aware operations and config file protection.
pub mod path_security;
/// Replace tool core: input parsing, edit application, output rendering.
pub mod replace;
/// Shared symbol types and helpers for handler and file_tools.
mod symbols;
/// Workspace root synchronization for PreToolUse hook requests.
pub mod sync_roots_server;
/// Transformation layer trait between protocol boundaries and LSP.
pub mod tool_server;

pub use diagnostics_server::DiagnosticsServer;
pub use document_manager::{DocumentManager, DocumentNotification};
pub use handler::LspBridgeHandler;
pub use path_security::PathValidator;
pub use sync_roots_server::SyncRootsServer;
pub use tool_server::ToolServer;

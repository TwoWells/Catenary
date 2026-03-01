// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

/// Manages document lifecycle and sync between disk and LSP servers.
mod document_manager;
/// Glob tool handler: unified file/directory/pattern browsing.
mod file_tools;
/// Maps MCP tool calls to LSP requests.
mod handler;
/// Path validation for LSP-aware operations and config file protection.
pub mod path_security;

pub use document_manager::{DocumentManager, DocumentNotification};
pub use handler::LspBridgeHandler;
pub use path_security::PathValidator;

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

/// Manages document lifecycle and sync between disk and LSP servers.
mod document_manager;
/// File I/O tool handlers.
mod file_tools;
/// Maps MCP tool calls to LSP requests.
mod handler;
/// Path validation and security for file I/O tools.
pub mod path_security;

pub use document_manager::{DocumentManager, DocumentNotification};
pub use handler::LspBridgeHandler;
pub use path_security::PathValidator;

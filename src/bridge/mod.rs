/*
 * Copyright (C) 2026 Mark Wells Dev
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

/// Manages document lifecycle and sync between disk and LSP servers.
mod document_manager;
/// File I/O tool handlers.
mod file_tools;
/// Maps MCP tool calls to LSP requests.
mod handler;
/// Path validation and security for file I/O tools.
pub mod path_security;
/// Shell execution tool with allowlist enforcement.
pub mod run_tool;

pub use document_manager::{DocumentManager, DocumentNotification};
pub use handler::LspBridgeHandler;
pub use path_security::PathValidator;

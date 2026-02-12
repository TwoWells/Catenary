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

//! Catenary is a bridge between MCP (Model Context Protocol) and LSP (Language Server Protocol).
//!
//! It allows AI coding assistants to access IDE-quality code intelligence by multiplexing
//! multiple language servers and exposing their capabilities via MCP tools.

/// Bridge logic between MCP and LSP.
pub mod bridge;
/// Command-line interface definitions and utilities.
pub mod cli;
/// Configuration handling for language servers and session settings.
pub mod config;
/// LSP client implementation and server management.
pub mod lsp;
/// MCP server implementation and type definitions.
pub mod mcp;
/// Session management and event broadcasting.
pub mod session;

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

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
/// SQLite database connection management, schema creation, and migrations.
pub mod db;
/// Diagnostic noise filtering for LSP server output.
pub mod filter;
/// Grammar installation, listing, and removal for tree-sitter integration.
pub mod install;
/// Logger trait for capturing protocol events.
pub mod logger;
/// LSP client implementation and server management.
pub mod lsp;
/// MCP server implementation and type definitions.
pub mod mcp;
/// IPC server for file-change notifications from hooks.
pub mod notify;
/// Session management and event broadcasting.
pub mod session;
/// Interactive TUI for session browsing and event tailing.
pub mod tui;

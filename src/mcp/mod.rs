// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

/// MCP server implementation over stdin/stdout.
mod server;
/// MCP type definitions and JSON-RPC messages.
mod types;

pub use server::{McpServer, RootsChangedCallback, ToolHandler};
pub use types::*;

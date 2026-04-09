// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Server definitions — how to run and configure a language server.

use serde::Deserialize;

/// Server definition — how to run and configure a language server.
///
/// Defined in `[server.*]` config sections, referenced by name from
/// `[language.*]` entries. This is adapter-level config consumed by
/// the LSP client layer — the routing core never sees it directly.
#[derive(Debug, Deserialize, Clone)]
pub struct ServerDef {
    /// The command to execute (e.g., "rust-analyzer", "clangd").
    pub command: String,

    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,

    /// Initialization options to pass to the LSP server.
    #[serde(default)]
    pub initialization_options: Option<serde_json::Value>,

    /// Server-specific settings returned in `workspace/configuration`
    /// responses.
    #[serde(default)]
    pub settings: Option<serde_json::Value>,
}

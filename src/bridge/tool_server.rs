// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Transformation layer between protocol boundaries and LSP.
//!
//! `ToolServer` implementations receive application-level params, do work
//! using `LspClient`, and return results. They do not log protocol messages
//! — the boundary components on either side handle logging. A `ToolServer`
//! is a black box: what went in and what came out are linked by `parent_id`
//! at the protocol level.

/// Transformation layer between protocol boundaries and LSP.
///
/// Implementations receive application-level params, do work using
/// `LspClient`, and return results. They do not log protocol messages
/// — the boundary components on either side handle logging. A
/// `ToolServer` is a black box: what went in and what came out are
/// linked by `parent_id` at the protocol level.
#[allow(async_fn_in_trait, reason = "no dyn dispatch — only concrete types")]
pub trait ToolServer: Send + Sync {
    /// Execute the tool with the given parameters.
    ///
    /// `parent_id` is the database `id` of the entry-point protocol
    /// message that triggered this execution. Implementations pass it
    /// through to `LspClient` request methods so LSP messages are
    /// correlated with their triggering MCP/hook request.
    ///
    /// # Errors
    ///
    /// Returns an error if the tool execution fails.
    async fn execute(
        &self,
        params: &serde_json::Value,
        parent_id: Option<i64>,
    ) -> anyhow::Result<serde_json::Value>;
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Protocol categorization and collapse key computation for the display pipeline.
//!
//! Pure functions that map protocol messages to grouping labels and collapse
//! keys. Categories follow the LSP and MCP specs' own method groupings —
//! explicit `match` on method strings, no regex or prefix matching.

use crate::session::SessionMessage;

// ── Category functions ───────────────────────────────────────────────────

/// Categorize an LSP method.
#[must_use]
pub fn lsp_category(method: &str) -> &'static str {
    match method {
        // lifecycle
        "initialize"
        | "initialized"
        | "shutdown"
        | "exit"
        | "client/registerCapability"
        | "client/unregisterCapability"
        | "$/setTrace"
        | "$/logTrace" => "lifecycle",

        // sync
        "textDocument/didOpen"
        | "textDocument/didChange"
        | "textDocument/didSave"
        | "textDocument/didClose"
        | "textDocument/willSave"
        | "textDocument/willSaveWaitUntil" => "sync",

        // language
        "textDocument/hover"
        | "textDocument/definition"
        | "textDocument/references"
        | "textDocument/rename"
        | "textDocument/prepareRename"
        | "textDocument/implementation"
        | "textDocument/typeDefinition"
        | "textDocument/declaration"
        | "textDocument/codeAction"
        | "textDocument/documentSymbol"
        | "textDocument/completion"
        | "textDocument/signatureHelp"
        | "textDocument/formatting"
        | "textDocument/rangeFormatting"
        | "textDocument/diagnostic"
        | "textDocument/codeLens"
        | "textDocument/documentHighlight"
        | "textDocument/foldingRange"
        | "textDocument/selectionRange"
        | "textDocument/linkedEditingRange"
        | "textDocument/semanticTokens/full"
        | "textDocument/semanticTokens/range"
        | "callHierarchy/incomingCalls"
        | "callHierarchy/outgoingCalls"
        | "textDocument/prepareCallHierarchy"
        | "typeHierarchy/subtypes"
        | "typeHierarchy/supertypes"
        | "textDocument/prepareTypeHierarchy"
        | "workspaceSymbol/resolve" => "language",

        // window
        "window/logMessage" | "window/showMessage" | "window/workDoneProgress/create" => "window",

        // workspace
        "workspace/symbol"
        | "workspace/configuration"
        | "workspace/didChangeConfiguration"
        | "workspace/didChangeWatchedFiles"
        | "workspace/didChangeWorkspaceFolders" => "workspace",

        // progress
        "$/progress" => "progress",

        _ => "unknown",
    }
}

/// Categorize an MCP method.
#[must_use]
pub fn mcp_category(method: &str) -> &'static str {
    match method {
        "initialize" | "notifications/initialized" => "init",
        "tools/list" | "tools/call" => "tools",
        "roots/list" | "notifications/roots/list_changed" => "roots",
        "notifications/cancelled" => "cancelled",
        _ => "unknown",
    }
}

/// Categorize a hook method.
#[must_use]
pub fn hook_category(method: &str) -> &'static str {
    match method {
        "PostToolUse" => "diagnostics",
        "PreToolUse" => "sync",
        "SessionStart" => "lifecycle",
        _ => "unknown",
    }
}

// ── Collapse key ─────────────────────────────────────────────────────────

/// Returns a collapse key for run grouping, or `None` if the message
/// should never collapse.
#[must_use]
pub fn collapse_key(msg: &SessionMessage) -> Option<String> {
    match msg.r#type.as_str() {
        "lsp" => {
            let cat = lsp_category(&msg.method);
            match cat {
                "progress" => {
                    let token = extract_progress_token(&msg.payload).unwrap_or_default();
                    Some(format!("progress:{}:{token}", msg.server))
                }
                "window" => {
                    let level = extract_log_level(&msg.payload)?;
                    if level >= 3 {
                        Some(format!("log:{}:{level}", msg.server))
                    } else {
                        // Errors and warnings never collapse.
                        None
                    }
                }
                "sync" => {
                    let uri = extract_sync_uri(&msg.payload).unwrap_or_default();
                    Some(format!("sync:{}:{uri}", msg.server))
                }
                "lifecycle" => Some(format!("lifecycle:{}", msg.server)),
                _ => Some(format!(
                    "proto:{}:{}:{}:{}",
                    msg.r#type, msg.server, msg.client, msg.method
                )),
            }
        }
        "mcp" => {
            let cat = mcp_category(&msg.method);
            match cat {
                "init" => Some("init:mcp".to_string()),
                _ => Some(format!(
                    "proto:{}:{}:{}:{}",
                    msg.r#type, msg.server, msg.client, msg.method
                )),
            }
        }
        // All hook messages and anything else → never collapse.
        _ => None,
    }
}

// ── Payload extraction helpers ───────────────────────────────────────────

/// Extract progress token from a `$/progress` payload.
///
/// The token can be a string or a number (per LSP spec).
fn extract_progress_token(payload: &serde_json::Value) -> Option<String> {
    let token = payload.get("token")?;
    token
        .as_str()
        .map(String::from)
        .or_else(|| token.as_u64().map(|n| n.to_string()))
        .or_else(|| token.as_i64().map(|n| n.to_string()))
}

/// Extract log level (`MessageType`) from a `window/logMessage` payload.
///
/// `MessageType` enum: 1=error, 2=warning, 3=info, 4=log.
fn extract_log_level(payload: &serde_json::Value) -> Option<u32> {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "MessageType values are 1-4"
    )]
    payload.get("type")?.as_u64().map(|n| n as u32)
}

/// Extract the document URI from a sync notification payload.
fn extract_sync_uri(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("textDocument")?
        .get("uri")?
        .as_str()
        .map(String::from)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::session::SessionMessage;

    fn make_message(r#type: &str, method: &str, server: &str) -> SessionMessage {
        SessionMessage {
            id: 0,
            r#type: r#type.to_string(),
            method: method.to_string(),
            server: server.to_string(),
            client: "catenary".to_string(),
            request_id: None,
            parent_id: None,
            timestamp: chrono::Utc::now(),
            payload: serde_json::json!({}),
        }
    }

    fn make_message_with_payload(
        r#type: &str,
        method: &str,
        server: &str,
        payload: serde_json::Value,
    ) -> SessionMessage {
        SessionMessage {
            id: 0,
            r#type: r#type.to_string(),
            method: method.to_string(),
            server: server.to_string(),
            client: "catenary".to_string(),
            request_id: None,
            parent_id: None,
            timestamp: chrono::Utc::now(),
            payload,
        }
    }

    // ── Category tests ───────────────────────────────────────────────────

    #[test]
    fn test_lsp_category_hover() {
        assert_eq!(lsp_category("textDocument/hover"), "language");
    }

    #[test]
    fn test_lsp_category_progress() {
        assert_eq!(lsp_category("$/progress"), "progress");
    }

    #[test]
    fn test_lsp_category_did_open() {
        assert_eq!(lsp_category("textDocument/didOpen"), "sync");
    }

    #[test]
    fn test_lsp_category_unknown() {
        assert_eq!(lsp_category("custom/unknownMethod"), "unknown");
    }

    #[test]
    fn test_mcp_category_tools_call() {
        assert_eq!(mcp_category("tools/call"), "tools");
    }

    #[test]
    fn test_mcp_category_initialize() {
        assert_eq!(mcp_category("initialize"), "init");
    }

    #[test]
    fn test_hook_category_posttooluse() {
        assert_eq!(hook_category("PostToolUse"), "diagnostics");
    }

    // ── Collapse key tests ───────────────────────────────────────────────

    #[test]
    fn test_collapse_key_progress() {
        let msg = make_message_with_payload(
            "lsp",
            "$/progress",
            "rust-analyzer",
            serde_json::json!({"token": "rust-analyzer/indexing"}),
        );
        let key = collapse_key(&msg);
        assert_eq!(
            key.as_deref(),
            Some("progress:rust-analyzer:rust-analyzer/indexing")
        );
    }

    #[test]
    fn test_collapse_key_sync() {
        let msg = make_message_with_payload(
            "lsp",
            "textDocument/didOpen",
            "rust-analyzer",
            serde_json::json!({"textDocument": {"uri": "file:///src/main.rs"}}),
        );
        let key = collapse_key(&msg);
        assert!(key.is_some());
        let key = key.expect("should have collapse key");
        assert!(
            key.starts_with("sync:"),
            "key should start with sync: got {key}"
        );
        assert!(
            key.contains("rust-analyzer"),
            "key should contain server: got {key}"
        );
    }

    #[test]
    fn test_collapse_key_hook_none() {
        let msg = make_message("hook", "PostToolUse", "catenary");
        assert!(collapse_key(&msg).is_none());
    }

    #[test]
    fn test_collapse_key_error_log_none() {
        let msg = make_message_with_payload(
            "lsp",
            "window/logMessage",
            "rust-analyzer",
            serde_json::json!({"type": 1}), // error
        );
        assert!(
            collapse_key(&msg).is_none(),
            "error-level log messages should not collapse"
        );
    }

    #[test]
    fn test_collapse_key_info_log() {
        let msg = make_message_with_payload(
            "lsp",
            "window/logMessage",
            "rust-analyzer",
            serde_json::json!({"type": 3}), // info
        );
        let key = collapse_key(&msg);
        assert!(key.is_some());
        let key = key.expect("should have collapse key");
        assert!(
            key.starts_with("log:"),
            "key should start with log: got {key}"
        );
    }
}

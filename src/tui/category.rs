// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Collapse key computation and display-specific helpers for the TUI pipeline.
//!
//! Category functions (`lsp_category`, `mcp_category`, `hook_category`) live in
//! [`crate::protocol::category`] and are re-exported here for convenience.

pub use crate::protocol::category::{hook_category, lsp_category, mcp_category};

use crate::session::SessionMessage;

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

// ── Collapsed run payload extractors ────────────────────────────────

/// Extract the progress title from a run of `$/progress` messages.
///
/// Looks for the first message with `value.kind == "begin"` and a `title`
/// field. Falls back to the progress token from the first message.
pub(crate) fn extract_progress_title(
    messages: &[SessionMessage],
    start: usize,
    end: usize,
) -> String {
    for msg in &messages[start..=end] {
        if let Some(value) = msg.payload.get("value")
            && value.get("kind").and_then(|k| k.as_str()) == Some("begin")
            && let Some(title) = value.get("title").and_then(|t| t.as_str())
        {
            return title.to_string();
        }
    }
    // Fall back to progress token from the first message.
    extract_progress_token(&messages[start].payload).unwrap_or_default()
}

/// Extract the percentage range from a run of `$/progress` messages.
///
/// Returns the first and last `value.percentage` values found in the run.
pub(crate) fn extract_progress_pct_range(
    messages: &[SessionMessage],
    start: usize,
    end: usize,
) -> (Option<u64>, Option<u64>) {
    let mut first = None;
    let mut last = None;
    for msg in &messages[start..=end] {
        if let Some(value) = msg.payload.get("value")
            && let Some(pct) = value.get("percentage").and_then(serde_json::Value::as_u64)
        {
            if first.is_none() {
                first = Some(pct);
            }
            last = Some(pct);
        }
    }
    (first, last)
}

/// Extract the file basename from a sync run's `textDocument.uri`.
pub(crate) fn extract_sync_basename(
    messages: &[SessionMessage],
    start: usize,
    end: usize,
) -> Option<String> {
    for msg in &messages[start..=end] {
        if let Some(uri) = extract_sync_uri(&msg.payload) {
            let name = std::path::Path::new(uri.as_str())
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&uri);
            return Some(name.to_string());
        }
    }
    None
}

/// Extract deduplicated operation labels from a sync run, preserving order.
///
/// Maps `didOpen` → `open`, `didChange` → `change`, etc.
pub(crate) fn extract_sync_operations(
    messages: &[SessionMessage],
    start: usize,
    end: usize,
) -> Vec<&'static str> {
    let mut ops: Vec<&'static str> = Vec::new();
    for msg in &messages[start..=end] {
        let label = match msg.method.as_str() {
            "textDocument/didOpen" => "open",
            "textDocument/didChange" => "change",
            "textDocument/didSave" => "save",
            "textDocument/didClose" => "close",
            _ => continue,
        };
        if !ops.contains(&label) {
            ops.push(label);
        }
    }
    ops
}

/// Map a log collapse key's level to a human-readable label.
///
/// Collapse key format: `log:{server}:{level}`.
/// Level 3 = info, level 4 = log.
pub(crate) fn log_level_label(collapse_key: &str) -> &'static str {
    match collapse_key.rsplit(':').next() {
        Some("3") => "info",
        _ => "log",
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::session::test_support::{message, message_with_payload};

    fn make_message(r#type: &str, method: &str, server: &str) -> SessionMessage {
        message(r#type, method, server)
    }

    fn make_message_with_payload(
        r#type: &str,
        method: &str,
        server: &str,
        payload: serde_json::Value,
    ) -> SessionMessage {
        message_with_payload(r#type, method, server, payload)
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
    fn test_hook_category_methods() {
        assert_eq!(hook_category("post-tool/diagnostics"), "diagnostics");
        assert_eq!(hook_category("pre-agent/turn-start"), "lifecycle");
        assert_eq!(hook_category("pre-tool/editing-state"), "lifecycle");
        assert_eq!(hook_category("pre-tool/check-command"), "lifecycle");
        assert_eq!(hook_category("post-agent/require-release"), "lifecycle");
        assert_eq!(hook_category("session-start/clear-editing"), "lifecycle");
        assert_eq!(hook_category("unknown/method"), "unknown");
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

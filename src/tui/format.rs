// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Message formatting helpers for the TUI.
//!
//! Styled and plain-text formatters for single messages, merged pairs,
//! and collapsed runs of protocol messages.

use ratatui::text::{Line, Span};

use super::category;
use super::icons::{IconSet, basename, diag_style, tool_icon};
use super::pipeline::{DisplayEntry, SegmentPosition};
use super::theme::Theme;
use crate::session::SessionMessage;

// ── Helpers ──────────────────────────────────────────────────────────────

/// Format a `started_at` timestamp as a human-readable duration.
#[must_use]
pub fn format_ago(started: chrono::DateTime<chrono::Utc>) -> String {
    let elapsed = chrono::Utc::now()
        .signed_duration_since(started)
        .num_seconds()
        .max(0);
    if elapsed < 60 {
        format!("{elapsed}s ago")
    } else if elapsed < 3600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86400 {
        format!("{}h ago", elapsed / 3600)
    } else {
        format!("{}d ago", elapsed / 86400)
    }
}

/// Determine direction arrow from a JSON-RPC payload.
///
/// If the payload has `"result"` or `"error"`, the message is inbound (`←`);
/// otherwise outbound (`→`).
fn message_direction_arrow(payload: &serde_json::Value) -> &'static str {
    if payload.get("result").is_some() || payload.get("error").is_some() {
        "\u{2190}" // ←
    } else {
        "\u{2192}" // →
    }
}

// ── Single message formatters ────────────────────────────────────────────

/// Build a styled [`Line`] for a protocol message.
#[must_use]
pub fn format_message_styled(
    msg: &SessionMessage,
    icons: &IconSet,
    theme: &Theme,
) -> Line<'static> {
    let ts = msg.timestamp.format("%H:%M:%S").to_string();
    let ts_span = Span::styled(format!("{ts}  "), theme.timestamp);

    match msg.r#type.as_str() {
        "lsp" => {
            let arrow = message_direction_arrow(&msg.payload);
            Line::from(vec![
                ts_span,
                Span::styled(format!("[{}] ", msg.server), theme.accent),
                Span::styled(format!("{arrow} "), theme.text),
                Span::styled(msg.method.clone(), theme.text),
            ])
        }
        "mcp" => {
            if msg.method == "tools/call" {
                let tool_name = msg
                    .payload
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or(&msg.method);
                let icon = tool_icon(tool_name, icons);
                Line::from(vec![
                    ts_span,
                    Span::styled(icon.to_string(), theme.success),
                    Span::styled(tool_name.to_string(), theme.text),
                ])
            } else {
                let arrow = message_direction_arrow(&msg.payload);
                Line::from(vec![
                    ts_span,
                    Span::styled("[mcp] ".to_string(), theme.text),
                    Span::styled(format!("{arrow} "), theme.text),
                    Span::styled(msg.method.clone(), theme.text),
                ])
            }
        }
        "hook" => {
            if let Some(count_val) = msg.payload.get("count") {
                let count = count_val.as_u64().unwrap_or(0);
                let file = msg
                    .payload
                    .get("file")
                    .and_then(|f| f.as_str())
                    .unwrap_or(&msg.method);
                let base = basename(file);
                if count == 0 {
                    Line::from(vec![
                        ts_span,
                        Span::styled(icons.diag_ok.clone(), theme.success),
                        Span::styled(base.to_string(), theme.text),
                    ])
                } else {
                    let preview = msg
                        .payload
                        .get("preview")
                        .and_then(|p| p.as_str())
                        .unwrap_or("");
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "diagnostic count is always small"
                    )]
                    let (icon, style) = diag_style(count as usize, preview, icons, theme);
                    let label = format!("{count} diagnostic{}", if count == 1 { "" } else { "s" });
                    Line::from(vec![
                        ts_span,
                        Span::styled(icon.to_string(), style),
                        Span::styled(format!("{base}: "), theme.text),
                        Span::styled(label, style),
                    ])
                }
            } else {
                Line::from(vec![
                    ts_span,
                    Span::styled("[hook] ".to_string(), theme.text),
                    Span::styled(msg.method.clone(), theme.text),
                ])
            }
        }
        other => Line::from(vec![
            ts_span,
            Span::styled(format!("[{other}] "), theme.text),
            Span::styled(msg.method.clone(), theme.text),
        ]),
    }
}

/// Plain-text message summary (used for filter matching).
#[must_use]
pub fn format_message_plain(msg: &SessionMessage) -> String {
    let ts = msg.timestamp.format("%H:%M:%S");

    match msg.r#type.as_str() {
        "lsp" => {
            let arrow = message_direction_arrow(&msg.payload);
            format!("{ts} [{}] {arrow} {}", msg.server, msg.method)
        }
        "mcp" => {
            if msg.method == "tools/call" {
                let tool_name = msg
                    .payload
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or(&msg.method);
                format!("{ts} {tool_name}")
            } else {
                let arrow = message_direction_arrow(&msg.payload);
                format!("{ts} [mcp] {arrow} {}", msg.method)
            }
        }
        "hook" => msg.payload.get("count").map_or_else(
            || format!("{ts} [hook] {}", msg.method),
            |count_val| {
                let count = count_val.as_u64().unwrap_or(0);
                let file = msg
                    .payload
                    .get("file")
                    .and_then(|f| f.as_str())
                    .unwrap_or(&msg.method);
                let base = basename(file);
                if count == 0 {
                    format!("{ts} {base}")
                } else {
                    format!("{ts} {base}: {count} diagnostics")
                }
            },
        ),
        other => format!("{ts} [{other}] {}", msg.method),
    }
}

// ── Duration + result helpers ────────────────────────────────────────────

/// Format a timing delta as a compact string.
///
/// Sub-10s: one decimal place (`0.5s`, `3.2s`).
/// 10s+: integer seconds (`12s`, `45s`).
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "millisecond timing values never exceed f64 mantissa range"
)]
pub fn format_duration_short(millis: i64) -> String {
    let millis = millis.max(0);
    if millis < 10_000 {
        let secs = millis as f64 / 1000.0;
        format!("{secs:.1}s")
    } else {
        format!("{}s", millis / 1000)
    }
}

/// Extract a result summary from a response payload.
///
/// Returns `"ok"` or `"error"` based on the JSON-RPC response structure.
fn result_summary(response: &SessionMessage) -> &'static str {
    let p = &response.payload;
    // MCP tools/call: check isError in result content
    if response.method == "tools/call"
        && let Some(result) = p.get("result")
    {
        if let Some(content) = result.get("content")
            && content
                .as_array()
                .and_then(|a| a.first())
                .and_then(|c| c.get("isError"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        {
            return "error";
        }
        if result
            .get("isError")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return "error";
        }
    }
    if p.get("error").is_some() {
        "error"
    } else {
        "ok"
    }
}

// ── Pair formatters ──────────────────────────────────────────────────────

/// Build a styled [`Line`] for a merged request/response pair.
///
/// Format: `HH:MM:SS [server] <-> method (result, Xs)`
///
/// Cancellations (`notifications/cancelled`) render with `x->`.
#[must_use]
pub fn format_pair_styled(
    request: &SessionMessage,
    response: &SessionMessage,
    icons: &IconSet,
    theme: &Theme,
) -> Line<'static> {
    let ts = request.timestamp.format("%H:%M:%S").to_string();
    let ts_span = Span::styled(format!("{ts}  "), theme.timestamp);

    let is_cancel = response.method == "notifications/cancelled";
    let arrow = if is_cancel { "x-> " } else { "<-> " };
    let arrow_style = if is_cancel { theme.error } else { theme.text };

    let delta_ms = response
        .timestamp
        .signed_duration_since(request.timestamp)
        .num_milliseconds();
    let timing = format_duration_short(delta_ms);

    let summary = if is_cancel {
        "cancelled"
    } else {
        result_summary(response)
    };
    let summary_style = if summary == "error" {
        theme.error
    } else {
        theme.muted
    };

    match request.r#type.as_str() {
        "lsp" => Line::from(vec![
            ts_span,
            Span::styled(format!("[{}] ", request.server), theme.accent),
            Span::styled(arrow.to_string(), arrow_style),
            Span::styled(request.method.clone(), theme.text),
            Span::styled(format!(" ({summary}, {timing})"), summary_style),
        ]),
        "mcp" => {
            if request.method == "tools/call" && !is_cancel {
                let tool_name = request
                    .payload
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or(&request.method);
                let icon = tool_icon(tool_name, icons);
                Line::from(vec![
                    ts_span,
                    Span::styled(icon.to_string(), theme.success),
                    Span::styled(tool_name.to_string(), theme.text),
                    Span::styled(format!(" ({summary}, {timing})"), summary_style),
                ])
            } else {
                let label = if request.method == "tools/call" {
                    request
                        .payload
                        .get("params")
                        .and_then(|p| p.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or(&request.method)
                } else {
                    &request.method
                };
                Line::from(vec![
                    ts_span,
                    Span::styled("[mcp] ".to_string(), theme.text),
                    Span::styled(arrow.to_string(), arrow_style),
                    Span::styled(label.to_string(), theme.text),
                    Span::styled(format!(" ({summary}, {timing})"), summary_style),
                ])
            }
        }
        other => Line::from(vec![
            ts_span,
            Span::styled(format!("[{other}] "), theme.text),
            Span::styled(arrow.to_string(), arrow_style),
            Span::styled(request.method.clone(), theme.text),
            Span::styled(format!(" ({summary}, {timing})"), summary_style),
        ]),
    }
}

/// Plain-text summary for a merged request/response pair (filter matching, yank).
#[must_use]
pub fn format_pair_plain(request: &SessionMessage, response: &SessionMessage) -> String {
    let ts = request.timestamp.format("%H:%M:%S");
    let is_cancel = response.method == "notifications/cancelled";
    let arrow = if is_cancel { "x->" } else { "<->" };
    let delta_ms = response
        .timestamp
        .signed_duration_since(request.timestamp)
        .num_milliseconds();
    let timing = format_duration_short(delta_ms);
    let summary = if is_cancel {
        "cancelled"
    } else {
        result_summary(response)
    };

    match request.r#type.as_str() {
        "lsp" => format!(
            "{ts} [{}] {arrow} {} ({summary}, {timing})",
            request.server, request.method
        ),
        "mcp" => {
            if request.method == "tools/call" && !is_cancel {
                let tool_name = request
                    .payload
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or(&request.method);
                format!("{ts} {tool_name} ({summary}, {timing})")
            } else {
                let label = if request.method == "tools/call" {
                    request
                        .payload
                        .get("params")
                        .and_then(|p| p.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or(&request.method)
                } else {
                    &request.method
                };
                format!("{ts} [mcp] {arrow} {label} ({summary}, {timing})")
            }
        }
        other => format!(
            "{ts} [{other}] {arrow} {} ({summary}, {timing})",
            request.method
        ),
    }
}

// ── Progress detail ──────────────────────────────────────────────────────

/// Format the detail portion of a collapsed progress run.
///
/// Combines message count with an optional percentage range.
fn format_progress_detail(count: usize, first_pct: Option<u64>, last_pct: Option<u64>) -> String {
    let count_label = format!("{count} message{}", if count == 1 { "" } else { "s" });
    match (first_pct, last_pct) {
        (Some(f), Some(l)) if f != l => format!("{count_label}, {f}%\u{2192}{l}%"),
        (Some(p), _) | (_, Some(p)) => format!("{count_label}, {p}%"),
        _ => count_label,
    }
}

// ── Collapsed run formatters ─────────────────────────────────────────────

/// Build a styled [`Line`] for a collapsed run of messages.
///
/// Category-specific rendering for progress, sync, lifecycle, log, and
/// MCP init runs. Falls back to generic `method (N messages)` for
/// protocol-level runs.
///
/// Uses the last message's timestamp (most recent in the run).
#[must_use]
pub fn format_collapsed_styled(
    messages: &[SessionMessage],
    start: usize,
    end: usize,
    count: usize,
    _icons: &IconSet,
    theme: &Theme,
) -> Line<'static> {
    let last = &messages[end];
    let ts = last.timestamp.format("%H:%M:%S").to_string();
    let ts_span = Span::styled(format!("{ts}  "), theme.timestamp);

    let key = category::collapse_key(&messages[start]);
    let key_str = key.as_deref().unwrap_or("");

    if key_str.starts_with("progress:") {
        let title = category::extract_progress_title(messages, start, end);
        let (first_pct, last_pct) = category::extract_progress_pct_range(messages, start, end);
        let detail = format_progress_detail(count, first_pct, last_pct);
        let server = &messages[start].server;
        Line::from(vec![
            ts_span,
            Span::styled(format!("[{server}] "), theme.accent),
            Span::styled(title, theme.text),
            Span::styled(format!(" ({detail})"), theme.muted),
        ])
    } else if key_str.starts_with("log:") {
        let label = category::log_level_label(key_str);
        let count_label = format!("{count} message{}", if count == 1 { "" } else { "s" });
        let server = &messages[start].server;
        Line::from(vec![
            ts_span,
            Span::styled(format!("[{server}] "), theme.accent),
            Span::styled(label.to_string(), theme.text),
            Span::styled(format!(" ({count_label})"), theme.muted),
        ])
    } else if key_str.starts_with("sync:") {
        let file = category::extract_sync_basename(messages, start, end).unwrap_or_default();
        let ops = category::extract_sync_operations(messages, start, end);
        let ops_str = ops.join(", ");
        let server = &messages[start].server;
        Line::from(vec![
            ts_span,
            Span::styled(format!("[{server}] "), theme.accent),
            Span::styled(format!("sync {file}"), theme.text),
            Span::styled(format!(" ({ops_str})"), theme.muted),
        ])
    } else if key_str.starts_with("lifecycle:") {
        let server = &messages[start].server;
        Line::from(vec![
            ts_span,
            Span::styled(format!("[{server}] "), theme.accent),
            Span::styled("initialized".to_string(), theme.text),
        ])
    } else if key_str == "init:mcp" {
        Line::from(vec![
            ts_span,
            Span::styled("mcp initialized".to_string(), theme.text),
        ])
    } else {
        // Generic fallback (proto: or unknown).
        let label = format!("{count} message{}", if count == 1 { "" } else { "s" });
        match last.r#type.as_str() {
            "lsp" => Line::from(vec![
                ts_span,
                Span::styled(format!("[{}] ", last.server), theme.accent),
                Span::styled(last.method.clone(), theme.text),
                Span::styled(format!(" ({label})"), theme.muted),
            ]),
            "mcp" => Line::from(vec![
                ts_span,
                Span::styled("[mcp] ".to_string(), theme.text),
                Span::styled(last.method.clone(), theme.text),
                Span::styled(format!(" ({label})"), theme.muted),
            ]),
            other => Line::from(vec![
                ts_span,
                Span::styled(format!("[{other}] "), theme.text),
                Span::styled(last.method.clone(), theme.text),
                Span::styled(format!(" ({label})"), theme.muted),
            ]),
        }
    }
}

/// Plain-text summary for a collapsed run (filter matching, yank).
///
/// Category-specific rendering matching [`format_collapsed_styled`].
#[must_use]
pub fn format_collapsed_plain(
    messages: &[SessionMessage],
    start: usize,
    end: usize,
    count: usize,
) -> String {
    let last = &messages[end];
    let ts = last.timestamp.format("%H:%M:%S");

    let key = category::collapse_key(&messages[start]);
    let key_str = key.as_deref().unwrap_or("");

    if key_str.starts_with("progress:") {
        let title = category::extract_progress_title(messages, start, end);
        let (first_pct, last_pct) = category::extract_progress_pct_range(messages, start, end);
        let detail = format_progress_detail(count, first_pct, last_pct);
        format!("{ts} [{}] {title} ({detail})", messages[start].server)
    } else if key_str.starts_with("log:") {
        let label = category::log_level_label(key_str);
        let count_label = format!("{count} message{}", if count == 1 { "" } else { "s" });
        format!("{ts} [{}] {label} ({count_label})", messages[start].server)
    } else if key_str.starts_with("sync:") {
        let file = category::extract_sync_basename(messages, start, end).unwrap_or_default();
        let ops = category::extract_sync_operations(messages, start, end);
        let ops_str = ops.join(", ");
        format!("{ts} [{}] sync {file} ({ops_str})", messages[start].server)
    } else if key_str.starts_with("lifecycle:") {
        format!("{ts} [{}] initialized", messages[start].server)
    } else if key_str == "init:mcp" {
        format!("{ts} mcp initialized")
    } else {
        // Generic fallback (proto: or unknown).
        let label = format!("{count} message{}", if count == 1 { "" } else { "s" });
        match last.r#type.as_str() {
            "lsp" => format!("{ts} [{}] {} ({label})", last.server, last.method),
            "mcp" => format!("{ts} [mcp] {} ({label})", last.method),
            other => format!("{ts} [{other}] {} ({label})", last.method),
        }
    }
}

// ── Scope formatters ──────────────────────────────────────────────────

/// Build a styled [`Line`] for a scope header (parent with grouped children).
///
/// Basic rendering: the parent's summary with child count appended.
/// For tool calls: `HH:MM:SS icon tool_name (N children, Xs)`.
/// The formatter rewrite (tickets 04a/04b) will enhance this with icons,
/// error extraction, and argument surfacing.
///
/// Segment position controls the ellipsis convention on tool names:
/// - `Only`: `tool_name (metrics)` — full scope, no ellipsis
/// - `First`: `tool_name…` — scope opened, no metrics yet
/// - `Middle`: `…tool_name…` — continuation, no metrics yet
/// - `Last`: `…tool_name (metrics)` — final segment with metrics
#[must_use]
pub fn format_scope_styled(
    parent: &DisplayEntry,
    child_count: usize,
    position: SegmentPosition,
    messages: &[SessionMessage],
    icons: &IconSet,
    theme: &Theme,
) -> Line<'static> {
    let children_label = format!(
        "{child_count} child{}",
        if child_count == 1 { "" } else { "ren" }
    );
    let has_metrics = matches!(position, SegmentPosition::Only | SegmentPosition::Last);

    match parent {
        DisplayEntry::Paired {
            request_index,
            response_index,
            ..
        } => {
            let req = &messages[*request_index];
            let resp = &messages[*response_index];
            let ts = req.timestamp.format("%H:%M:%S").to_string();
            let ts_span = Span::styled(format!("{ts}  "), theme.timestamp);

            let delta_ms = resp
                .timestamp
                .signed_duration_since(req.timestamp)
                .num_milliseconds();
            let timing = format_duration_short(delta_ms);

            if req.method == "tools/call" {
                let tool_name = req
                    .payload
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or(&req.method);
                let icon = tool_icon(tool_name, icons);
                let name = segment_tool_name(tool_name, position);
                let meta = if has_metrics {
                    format!(" ({children_label}, {timing})")
                } else {
                    format!(" ({children_label})")
                };
                Line::from(vec![
                    ts_span,
                    Span::styled(icon.to_string(), theme.success),
                    Span::styled(name, theme.text),
                    Span::styled(meta, theme.muted),
                ])
            } else {
                let method = segment_tool_name(&req.method, position);
                let meta = if has_metrics {
                    format!(" ({children_label}, {timing})")
                } else {
                    format!(" ({children_label})")
                };
                Line::from(vec![
                    ts_span,
                    Span::styled(format!("[{}] ", req.server), theme.accent),
                    Span::styled(method, theme.text),
                    Span::styled(meta, theme.muted),
                ])
            }
        }
        DisplayEntry::Single { index, .. } => {
            let msg = &messages[*index];
            let ts = msg.timestamp.format("%H:%M:%S").to_string();
            let ts_span = Span::styled(format!("{ts}  "), theme.timestamp);
            if msg.method == "tools/call" {
                let tool_name = msg
                    .payload
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or(&msg.method);
                let icon = tool_icon(tool_name, icons);
                let name = segment_tool_name(tool_name, position);
                Line::from(vec![
                    ts_span,
                    Span::styled(icon.to_string(), theme.success),
                    Span::styled(name, theme.text),
                    Span::styled(format!(" ({children_label})"), theme.muted),
                ])
            } else {
                let method = segment_tool_name(&msg.method, position);
                Line::from(vec![
                    ts_span,
                    Span::styled(method, theme.text),
                    Span::styled(format!(" ({children_label})"), theme.muted),
                ])
            }
        }
        // Collapsed and Scope parents are unlikely but handle uniformly.
        _ => {
            let label = format!("scope ({children_label})");
            Line::from(vec![Span::styled(label, theme.text)])
        }
    }
}

/// Apply the segment ellipsis convention to a tool/method name.
///
/// - `Only`: `name` (no ellipsis)
/// - `First`: `name…`
/// - `Middle`: `…name…`
/// - `Last`: `…name`
fn segment_tool_name(name: &str, position: SegmentPosition) -> String {
    match position {
        SegmentPosition::Only => name.to_string(),
        SegmentPosition::First => format!("{name}\u{2026}"),
        SegmentPosition::Middle => format!("\u{2026}{name}\u{2026}"),
        SegmentPosition::Last => format!("\u{2026}{name}"),
    }
}

/// Plain-text summary for a scope header (filter matching, yank).
#[must_use]
pub fn format_scope_plain(
    parent: &DisplayEntry,
    child_count: usize,
    position: SegmentPosition,
    messages: &[SessionMessage],
) -> String {
    let children_label = format!(
        "{child_count} child{}",
        if child_count == 1 { "" } else { "ren" }
    );
    let has_metrics = matches!(position, SegmentPosition::Only | SegmentPosition::Last);

    match parent {
        DisplayEntry::Paired {
            request_index,
            response_index,
            ..
        } => {
            let req = &messages[*request_index];
            let resp = &messages[*response_index];
            let ts = req.timestamp.format("%H:%M:%S");
            let delta_ms = resp
                .timestamp
                .signed_duration_since(req.timestamp)
                .num_milliseconds();
            let timing = format_duration_short(delta_ms);

            if req.method == "tools/call" {
                let tool_name = req
                    .payload
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or(&req.method);
                let name = segment_tool_name(tool_name, position);
                if has_metrics {
                    format!("{ts} {name} ({children_label}, {timing})")
                } else {
                    format!("{ts} {name} ({children_label})")
                }
            } else {
                let method = segment_tool_name(&req.method, position);
                if has_metrics {
                    format!(
                        "{ts} [{}] {method} ({children_label}, {timing})",
                        req.server
                    )
                } else {
                    format!("{ts} [{}] {method} ({children_label})", req.server)
                }
            }
        }
        DisplayEntry::Single { index, .. } => {
            let msg = &messages[*index];
            let ts = msg.timestamp.format("%H:%M:%S");
            if msg.method == "tools/call" {
                let tool_name = msg
                    .payload
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or(&msg.method);
                let name = segment_tool_name(tool_name, position);
                format!("{ts} {name} ({children_label})")
            } else {
                let method = segment_tool_name(&msg.method, position);
                format!("{ts} {method} ({children_label})")
            }
        }
        _ => format!("scope ({children_label})"),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use chrono::{TimeDelta, Utc};

    use crate::config::IconConfig;
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
            timestamp: Utc::now(),
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
            timestamp: Utc::now(),
            payload,
        }
    }

    #[test]
    fn test_format_ago_seconds() {
        let ts = Utc::now() - TimeDelta::seconds(30);
        assert_eq!(format_ago(ts), "30s ago");
    }

    #[test]
    fn test_format_ago_minutes() {
        let ts = Utc::now() - TimeDelta::minutes(5);
        assert_eq!(format_ago(ts), "5m ago");
    }

    #[test]
    fn test_format_ago_hours() {
        let ts = Utc::now() - TimeDelta::hours(2);
        assert_eq!(format_ago(ts), "2h ago");
    }

    // ── Message formatter tests ─────────────────────────────────────────

    #[test]
    fn test_format_message_styled_lsp() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let msg = make_message("lsp", "textDocument/hover", "rust-analyzer");
        let line = format_message_styled(&msg, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("[rust-analyzer]"),
            "should contain server name"
        );
        assert!(text.contains("textDocument/hover"), "should contain method");
        assert!(text.contains("\u{2192}"), "outbound request should show →");
    }

    #[test]
    fn test_format_message_styled_lsp_response() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let msg = make_message_with_payload(
            "lsp",
            "textDocument/hover",
            "rust-analyzer",
            serde_json::json!({"id": 1, "result": {"contents": "fn main()"}}),
        );
        let line = format_message_styled(&msg, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("\u{2190}"), "response should show ←");
    }

    #[test]
    fn test_format_message_styled_mcp() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let msg = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep"}}),
        );
        let line = format_message_styled(&msg, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("grep"), "should contain tool name");
    }

    #[test]
    fn test_format_message_styled_hook() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let msg = make_message_with_payload(
            "hook",
            "post-tool",
            "catenary",
            serde_json::json!({
                "file": "/src/lib.rs",
                "count": 2,
                "preview": "\t:12:1 [error] rustc: bad"
            }),
        );
        let line = format_message_styled(&msg, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("lib.rs"), "should contain file basename");
        assert!(text.contains("2 diagnostics"), "should show count");
    }

    #[test]
    fn test_format_message_styled_hook_clean() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let msg = make_message_with_payload(
            "hook",
            "post-tool",
            "catenary",
            serde_json::json!({"file": "/src/lib.rs", "count": 0}),
        );
        let line = format_message_styled(&msg, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("lib.rs"), "should contain file basename");
        assert!(
            line.spans.iter().any(|s| s.style == theme.success),
            "clean diagnostics should use success style"
        );
    }

    #[test]
    fn test_format_message_plain() {
        let msg = make_message("lsp", "textDocument/hover", "rust-analyzer");
        let plain = format_message_plain(&msg);
        assert!(plain.contains("[rust-analyzer]"));
        assert!(plain.contains("textDocument/hover"));
        assert!(plain.contains("\u{2192}"));

        let mcp_msg = make_message_with_payload(
            "mcp",
            "tools/call",
            "catenary",
            serde_json::json!({"params": {"name": "grep"}}),
        );
        let plain = format_message_plain(&mcp_msg);
        assert!(plain.contains("grep"));

        let hook_msg = make_message_with_payload(
            "hook",
            "post-tool",
            "catenary",
            serde_json::json!({"file": "/src/main.rs", "count": 3}),
        );
        let plain = format_message_plain(&hook_msg);
        assert!(plain.contains("main.rs"));
        assert!(plain.contains("3 diagnostics"));
    }

    // ── Collapsed rendering tests ────────────────────────────────────────

    #[test]
    fn test_format_collapsed_progress() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({"token": "ra/indexing"}),
            ),
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({"token": "ra/indexing"}),
            ),
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({"token": "ra/indexing"}),
            ),
        ];
        let line = format_collapsed_styled(&messages, 0, 2, 3, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("3 messages"),
            "should contain message count: {text}"
        );
        assert!(
            text.contains("[rust-analyzer]"),
            "should contain server name: {text}"
        );
    }

    #[test]
    fn test_format_collapsed_sync() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message_with_payload(
                "lsp",
                "textDocument/didOpen",
                "rust-analyzer",
                serde_json::json!({"textDocument": {"uri": "file:///src/main.rs"}}),
            ),
            make_message_with_payload(
                "lsp",
                "textDocument/didSave",
                "rust-analyzer",
                serde_json::json!({"textDocument": {"uri": "file:///src/main.rs"}}),
            ),
        ];
        let line = format_collapsed_styled(&messages, 0, 1, 2, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("sync main.rs"),
            "should contain sync + basename: {text}"
        );
        assert!(
            text.contains("open, save"),
            "should contain operations: {text}"
        );
    }

    // ── Category-specific collapsed rendering tests ─────────────────────

    #[test]
    fn test_format_collapsed_progress_with_title() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({
                    "token": "rust-analyzer/Roots Scanned",
                    "value": {"kind": "begin", "title": "Roots Scanned", "percentage": 0}
                }),
            ),
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({
                    "token": "rust-analyzer/Roots Scanned",
                    "value": {"kind": "report", "percentage": 13}
                }),
            ),
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({
                    "token": "rust-analyzer/Roots Scanned",
                    "value": {"kind": "report", "percentage": 49}
                }),
            ),
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({
                    "token": "rust-analyzer/Roots Scanned",
                    "value": {"kind": "end"}
                }),
            ),
        ];
        let line = format_collapsed_styled(&messages, 0, 3, 4, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("Roots Scanned"),
            "should contain title: {text}"
        );
        assert!(
            text.contains("0%\u{2192}49%"),
            "should contain percentage range: {text}"
        );

        let plain = format_collapsed_plain(&messages, 0, 3, 4);
        assert!(
            plain.contains("Roots Scanned"),
            "plain should contain title: {plain}"
        );
        assert!(
            plain.contains("0%\u{2192}49%"),
            "plain should contain percentage range: {plain}"
        );
    }

    #[test]
    fn test_format_collapsed_progress_no_percentage() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({
                    "token": "rust-analyzer/indexing",
                    "value": {"kind": "begin", "title": "Indexing"}
                }),
            ),
            make_message_with_payload(
                "lsp",
                "$/progress",
                "rust-analyzer",
                serde_json::json!({
                    "token": "rust-analyzer/indexing",
                    "value": {"kind": "end"}
                }),
            ),
        ];
        let line = format_collapsed_styled(&messages, 0, 1, 2, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Indexing"), "should contain title: {text}");
        assert!(text.contains("2 messages"), "should contain count: {text}");
        assert!(!text.contains('%'), "should not contain percentage: {text}");

        let plain = format_collapsed_plain(&messages, 0, 1, 2);
        assert!(
            plain.contains("Indexing"),
            "plain should contain title: {plain}"
        );
        assert!(
            !plain.contains('%'),
            "plain should not contain percentage: {plain}"
        );
    }

    #[test]
    fn test_format_collapsed_sync_operations() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message_with_payload(
                "lsp",
                "textDocument/didOpen",
                "rust-analyzer",
                serde_json::json!({"textDocument": {"uri": "file:///src/main.rs"}}),
            ),
            make_message_with_payload(
                "lsp",
                "textDocument/didSave",
                "rust-analyzer",
                serde_json::json!({"textDocument": {"uri": "file:///src/main.rs"}}),
            ),
        ];
        let line = format_collapsed_styled(&messages, 0, 1, 2, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("main.rs"), "should contain basename: {text}");
        assert!(text.contains("open"), "should contain open: {text}");
        assert!(text.contains("save"), "should contain save: {text}");

        let plain = format_collapsed_plain(&messages, 0, 1, 2);
        assert!(
            plain.contains("sync main.rs"),
            "plain should contain sync + basename: {plain}"
        );
        assert!(
            plain.contains("open, save"),
            "plain should contain operations: {plain}"
        );
    }

    #[test]
    fn test_format_collapsed_lifecycle() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message("lsp", "initialize", "shellscript"),
            make_message("lsp", "initialized", "shellscript"),
        ];
        let line = format_collapsed_styled(&messages, 0, 1, 2, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("initialized"),
            "should contain initialized: {text}"
        );
        assert!(
            text.contains("[shellscript]"),
            "should contain server name: {text}"
        );

        let plain = format_collapsed_plain(&messages, 0, 1, 2);
        assert!(
            plain.contains("initialized"),
            "plain should contain initialized: {plain}"
        );
        assert!(
            plain.contains("[shellscript]"),
            "plain should contain server name: {plain}"
        );
    }

    #[test]
    fn test_format_collapsed_mcp_init() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message("mcp", "initialize", "catenary"),
            make_message("mcp", "notifications/initialized", "catenary"),
        ];
        let line = format_collapsed_styled(&messages, 0, 1, 2, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("mcp initialized"),
            "should contain mcp initialized: {text}"
        );

        let plain = format_collapsed_plain(&messages, 0, 1, 2);
        assert!(
            plain.contains("mcp initialized"),
            "plain should contain mcp initialized: {plain}"
        );
    }

    #[test]
    fn test_format_collapsed_log_info() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message_with_payload(
                "lsp",
                "window/logMessage",
                "python",
                serde_json::json!({"type": 3, "message": "Loading..."}),
            ),
            make_message_with_payload(
                "lsp",
                "window/logMessage",
                "python",
                serde_json::json!({"type": 3, "message": "Ready."}),
            ),
            make_message_with_payload(
                "lsp",
                "window/logMessage",
                "python",
                serde_json::json!({"type": 3, "message": "Done."}),
            ),
        ];
        let line = format_collapsed_styled(&messages, 0, 2, 3, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("info"), "should contain info label: {text}");
        assert!(text.contains("3 messages"), "should contain count: {text}");

        let plain = format_collapsed_plain(&messages, 0, 2, 3);
        assert!(plain.contains("info"), "plain should contain info: {plain}");
        assert!(
            plain.contains("[python]"),
            "plain should contain server: {plain}"
        );
    }

    #[test]
    fn test_format_collapsed_generic_fallback() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let messages = vec![
            make_message_with_payload(
                "lsp",
                "workspace/configuration",
                "rust-analyzer",
                serde_json::json!({"id": 1}),
            ),
            make_message_with_payload(
                "lsp",
                "workspace/configuration",
                "rust-analyzer",
                serde_json::json!({"id": 2}),
            ),
        ];
        let line = format_collapsed_styled(&messages, 0, 1, 2, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("2 messages"),
            "should contain message count: {text}"
        );
        assert!(
            text.contains("workspace/configuration"),
            "should contain method: {text}"
        );

        let plain = format_collapsed_plain(&messages, 0, 1, 2);
        assert!(
            plain.contains("2 messages"),
            "plain should contain count: {plain}"
        );
        assert!(
            plain.contains("workspace/configuration"),
            "plain should contain method: {plain}"
        );
    }
}

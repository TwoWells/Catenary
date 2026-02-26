// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Theme, icon set, and event formatting helpers for the TUI.
//!
//! All colors use the terminal's ANSI palette so the TUI automatically
//! inherits whatever theme the user has configured.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::config::{IconConfig, IconPreset};
use crate::session::{EventKind, SessionEvent};

// ── Theme ────────────────────────────────────────────────────────────────

/// Semantic color theme that defers to the terminal's ANSI palette.
///
/// Uses only base ANSI colors (`Color::Green`, `Color::Red`, etc.) and
/// modifiers (`DIM`, `BOLD`, `REVERSED`) so the TUI automatically inherits
/// whatever theme the user has configured in their terminal emulator.
pub struct Theme {
    /// Style for the focused pane border.
    pub border_focused: Style,
    /// Style for the unfocused pane border.
    pub border_unfocused: Style,
    /// Style for pane titles.
    pub title: Style,
    /// Style for hint keybinding labels.
    pub hint_key: Style,
    /// Style for hint description text.
    pub hint_label: Style,
    /// Style for the selection highlight.
    pub selection: Style,

    /// Style for active sessions.
    pub session_active: Style,
    /// Style for dead sessions.
    pub session_dead: Style,
    /// Style for session metadata (language list, etc.).
    pub session_meta: Style,

    /// Style for timestamps.
    pub timestamp: Style,
    /// Style for normal text.
    pub text: Style,
    /// Style for accented text (language names, etc.).
    pub accent: Style,
    /// Style for success indicators.
    pub success: Style,
    /// Style for error indicators.
    pub error: Style,
    /// Style for warning indicators.
    pub warning: Style,
    /// Style for informational indicators.
    pub info: Style,
    /// Style for muted/dimmed text.
    pub muted: Style,
    /// Style for lock acquired events.
    pub lock: Style,
    /// Style for lock released events.
    pub unlock: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self::new()
    }
}

impl Theme {
    /// Build the default theme from the terminal's palette.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            border_focused: Style::new(),
            border_unfocused: Style::new().add_modifier(Modifier::DIM),
            title: Style::new().add_modifier(Modifier::BOLD),
            hint_key: Style::new().add_modifier(Modifier::BOLD),
            hint_label: Style::new().add_modifier(Modifier::DIM),
            selection: Style::new().add_modifier(Modifier::REVERSED),

            session_active: Style::new().fg(Color::Green),
            session_dead: Style::new().add_modifier(Modifier::DIM),
            session_meta: Style::new().add_modifier(Modifier::DIM),

            timestamp: Style::new().add_modifier(Modifier::DIM),
            text: Style::new(),
            accent: Style::new().fg(Color::Cyan),
            success: Style::new().fg(Color::Green),
            error: Style::new().fg(Color::Red),
            warning: Style::new().fg(Color::Yellow),
            info: Style::new().fg(Color::Blue),
            muted: Style::new().add_modifier(Modifier::DIM),
            lock: Style::new().fg(Color::Yellow),
            unlock: Style::new().fg(Color::Cyan),
        }
    }
}

// ── Icon set ─────────────────────────────────────────────────────────────

/// Resolved icon set with all values as owned strings.
///
/// Built from [`IconConfig`] by applying per-icon overrides on top of the
/// chosen preset defaults.
pub struct IconSet {
    /// Diagnostic error icon.
    pub diag_error: String,
    /// Diagnostic warning icon.
    pub diag_warn: String,
    /// Diagnostic info icon.
    pub diag_info: String,
    /// Diagnostic ok (clean) icon.
    pub diag_ok: String,
    /// Lock acquired icon.
    pub lock: String,
    /// Lock released icon.
    pub unlock: String,
    /// Search tool icon.
    pub tool_search: String,
    /// Codebase map tool icon.
    pub tool_map: String,
    /// Hover tool icon.
    pub tool_hover: String,
    /// Go-to-definition tool icon.
    pub tool_goto: String,
    /// Find references tool icon.
    pub tool_refs: String,
    /// Diagnostics tool icon.
    pub tool_diagnostics: String,
    /// Default (fallback) tool icon.
    pub tool_default: String,
}

impl IconSet {
    /// Resolve an [`IconConfig`] into a fully populated [`IconSet`].
    #[must_use]
    pub fn from_config(config: IconConfig) -> Self {
        let (unicode, nerd) = Self::preset_defaults();
        let base = match config.preset {
            IconPreset::Unicode => &unicode,
            IconPreset::Nerd => &nerd,
        };
        Self {
            diag_error: config.diag_error.unwrap_or_else(|| base.0.to_string()),
            diag_warn: config.diag_warn.unwrap_or_else(|| base.1.to_string()),
            diag_info: config.diag_info.unwrap_or_else(|| base.2.to_string()),
            diag_ok: config.diag_ok.unwrap_or_else(|| base.3.to_string()),
            lock: config.lock.unwrap_or_else(|| base.4.to_string()),
            unlock: config.unlock.unwrap_or_else(|| base.5.to_string()),
            tool_search: config.tool_search.unwrap_or_else(|| base.6.to_string()),
            tool_map: config.tool_map.unwrap_or_else(|| base.7.to_string()),
            tool_hover: config.tool_hover.unwrap_or_else(|| base.8.to_string()),
            tool_goto: config.tool_goto.unwrap_or_else(|| base.9.to_string()),
            tool_refs: config.tool_refs.unwrap_or_else(|| base.10.to_string()),
            tool_diagnostics: config
                .tool_diagnostics
                .unwrap_or_else(|| base.11.to_string()),
            tool_default: config.tool_default.unwrap_or_else(|| base.12.to_string()),
        }
    }

    /// Returns `(unicode_defaults, nerd_defaults)` tuples.
    ///
    /// Order: `diag_error`, `diag_warn`, `diag_info`, `diag_ok`, `lock`, `unlock`,
    ///        `tool_search`, `tool_map`, `tool_hover`, `tool_goto`, `tool_refs`,
    ///        `tool_diagnostics`, `tool_default`.
    #[allow(
        clippy::type_complexity,
        reason = "private helper returning preset tuples"
    )]
    const fn preset_defaults() -> (
        (
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
        ),
        (
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
        ),
    ) {
        let unicode = (
            "\u{2717} ", // ✗
            "\u{26A0} ", // ⚠
            "\u{2139} ", // ℹ
            "\u{2713} ", // ✓
            "\u{25B6} ", // ▶
            "\u{25C0} ", // ◀
            "\u{2192} ", // →
            "\u{2192} ", // →
            "\u{2192} ", // →
            "\u{2192} ", // →
            "\u{2192} ", // →
            "\u{2192} ", // →
            "\u{2192} ", // →
        );
        let nerd = (
            " ",         // nf-cod-error
            " ",         // nf-cod-warning
            " ",         // nf-cod-info
            " ",         // nf-cod-check
            " ",         // nf-cod-lock
            " ",         // nf-cod-unlock
            " ",         // nf-cod-search
            " ",         // nf-cod-map
            " ",         // nf-cod-comment_discussion
            " ",         // nf-cod-symbol_method
            " ",         // nf-cod-references
            " ",         // nf-fa-stethoscope
            "\u{2192} ", // → (no nerd equivalent)
        );
        (unicode, nerd)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Collapse consecutive progress events with the same `(language, title)`
/// into just the last event of each run.
#[must_use]
pub fn collapse_progress(events: Vec<&SessionEvent>) -> Vec<&SessionEvent> {
    let mut result: Vec<&SessionEvent> = Vec::with_capacity(events.len());
    for ev in events {
        if let EventKind::Progress {
            language, title, ..
        } = &ev.kind
            && let Some(last) = result.last()
            && let EventKind::Progress {
                language: prev_lang,
                title: prev_title,
                ..
            } = &last.kind
            && prev_lang == language
            && prev_title == title
        {
            result.pop();
        }
        result.push(ev);
    }
    result
}

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

/// Plain-text event summary (used for filter matching).
#[must_use]
pub fn format_event_plain(ev: &SessionEvent) -> String {
    let ts = ev.timestamp.format("%H:%M:%S");
    match &ev.kind {
        EventKind::Started => format!("{ts} session started"),
        EventKind::Shutdown => format!("{ts} session shutdown"),
        EventKind::ServerState { language, state } => format!("{ts} {language} {state}"),
        EventKind::Progress {
            language, title, ..
        } => format!("{ts} {language} {title}"),
        EventKind::ProgressEnd { language } => format!("{ts} {language} complete"),
        EventKind::ToolCall { tool, file } => {
            format!("{ts} {tool} {}", file.as_deref().unwrap_or(""))
        }
        EventKind::ToolResult { tool, success, .. } => {
            format!("{ts} {tool} {}", if *success { "ok" } else { "error" })
        }
        EventKind::Diagnostics {
            file,
            count,
            preview,
        } => format!("{ts} {file} {count} {preview}"),
        EventKind::McpMessage { direction, .. } => format!("{ts} mcp {direction}"),
        EventKind::LockAcquired { file, owner, .. } => format!("{ts} lock {file} {owner}"),
        EventKind::LockReleased { file, owner, .. } => format!("{ts} unlock {file} {owner}"),
        EventKind::LockDenied {
            file,
            owner,
            held_by,
        } => format!("{ts} denied {file} {owner} {held_by}"),
    }
}

/// Choose an icon for a tool call based on the tool name.
#[must_use]
pub fn tool_icon<'a>(name: &str, icons: &'a IconSet) -> &'a str {
    match name {
        "search" => &icons.tool_search,
        "codebase_map" => &icons.tool_map,
        "hover" => &icons.tool_hover,
        "definition" | "type_definition" => &icons.tool_goto,
        "find_references" | "call_hierarchy" | "type_hierarchy" => &icons.tool_refs,
        "diagnostics" => &icons.tool_diagnostics,
        _ => &icons.tool_default,
    }
}

/// Extract the basename from a file path.
#[must_use]
pub fn basename(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

/// Determine the diagnostic icon and style from the count and preview text.
#[must_use]
pub fn diag_style<'a>(
    count: usize,
    preview: &str,
    icons: &'a IconSet,
    theme: &Theme,
) -> (&'a str, Style) {
    if count == 0 {
        return (&icons.diag_ok, theme.success);
    }
    let lower = preview.to_lowercase();
    if lower.contains("[error]") {
        (&icons.diag_error, theme.error)
    } else if lower.contains("[warning]") {
        (&icons.diag_warn, theme.warning)
    } else if lower.contains("[info]") || lower.contains("[hint]") {
        (&icons.diag_info, theme.info)
    } else {
        (&icons.diag_warn, theme.warning)
    }
}

/// Build a styled [`Line`] for a single event.
#[must_use]
#[allow(clippy::too_many_lines, reason = "match arms for each event kind")]
pub fn format_event_styled(ev: &SessionEvent, icons: &IconSet, theme: &Theme) -> Line<'static> {
    let ts = ev.timestamp.format("%H:%M:%S").to_string();
    let ts_span = Span::styled(format!("{ts}  "), theme.timestamp);

    match &ev.kind {
        EventKind::Started => Line::from(vec![
            ts_span,
            Span::styled("● ", theme.success),
            Span::styled("session started", theme.text),
        ]),
        EventKind::Shutdown => Line::from(vec![
            ts_span,
            Span::styled("○ ", theme.muted),
            Span::styled("session shutdown", theme.text),
        ]),
        EventKind::ServerState { language, state } => Line::from(vec![
            ts_span,
            Span::styled("◆ ", theme.accent),
            Span::styled(format!("[{language}] "), theme.accent),
            Span::styled(format!("state → {state}"), theme.text),
        ]),
        EventKind::Progress {
            language,
            title,
            message,
            percentage,
        } => {
            let pct = percentage.map_or(String::new(), |p| format!(" ({p}%)"));
            let msg = message.as_ref().map_or(String::new(), |m| format!(": {m}"));
            Line::from(vec![
                ts_span,
                Span::styled("⟳ ", theme.text),
                Span::styled(format!("[{language}] "), theme.accent),
                Span::styled(format!("{title}{msg}{pct}"), theme.text),
            ])
        }
        EventKind::ProgressEnd { language } => Line::from(vec![
            ts_span,
            Span::styled("⟳ ", theme.text),
            Span::styled(format!("[{language}] "), theme.accent),
            Span::styled("complete", theme.text),
        ]),
        EventKind::ToolCall { tool, file } => {
            let icon = tool_icon(tool, icons);
            let file_str = file
                .as_ref()
                .map(|f| format!(" {}", basename(f)))
                .unwrap_or_default();
            Line::from(vec![
                ts_span,
                Span::styled(icon.to_string(), theme.success),
                Span::styled(format!("{tool}{file_str}"), theme.text),
            ])
        }
        EventKind::ToolResult {
            tool,
            success,
            duration_ms,
        } => {
            let (status_text, status_style) = if *success {
                ("ok", theme.success)
            } else {
                ("error", theme.error)
            };
            Line::from(vec![
                ts_span,
                Span::styled("← ", theme.info),
                Span::styled(format!("{tool} → "), theme.text),
                Span::styled(status_text.to_string(), status_style),
                Span::styled(format!(" ({duration_ms}ms)"), theme.text),
            ])
        }
        EventKind::Diagnostics {
            file,
            count,
            preview,
        } => {
            let (icon, style) = diag_style(*count, preview, icons, theme);
            let base = basename(file);
            if *count == 0 {
                Line::from(vec![
                    ts_span,
                    Span::styled(icon.to_string(), style),
                    Span::styled(base.to_string(), theme.text),
                ])
            } else {
                let label = format!("{count} diagnostic{}", if *count == 1 { "" } else { "s" });
                Line::from(vec![
                    ts_span,
                    Span::styled(icon.to_string(), style),
                    Span::styled(format!("{base}: "), theme.text),
                    Span::styled(label, style),
                ])
            }
        }
        EventKind::McpMessage { direction, .. } => {
            let arrow = if direction == "in" { "→" } else { "←" };
            Line::from(vec![
                ts_span,
                Span::styled(format!("◇ mcp {arrow}"), theme.muted),
            ])
        }
        EventKind::LockAcquired { file, owner, tool } => {
            let base = basename(file);
            let tool_label = tool.as_ref().map_or(String::new(), |t| format!(" ({t})"));
            Line::from(vec![
                ts_span,
                Span::styled(icons.lock.clone(), theme.lock),
                Span::styled(format!("{base}{tool_label} by {owner}"), theme.text),
            ])
        }
        EventKind::LockReleased { file, owner, tool } => {
            let base = basename(file);
            let tool_label = tool.as_ref().map_or(String::new(), |t| format!(" ({t})"));
            Line::from(vec![
                ts_span,
                Span::styled(icons.unlock.clone(), theme.unlock),
                Span::styled(format!("{base}{tool_label} by {owner}"), theme.unlock),
            ])
        }
        EventKind::LockDenied {
            file,
            owner,
            held_by,
        } => {
            let base = basename(file);
            let lock = &icons.lock;
            Line::from(vec![
                ts_span,
                Span::styled(format!("{lock}denied "), theme.error),
                Span::styled(
                    format!("{base} for {owner} (held by {held_by})"),
                    theme.text,
                ),
            ])
        }
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
    use crate::session::{EventKind, SessionEvent};

    fn make_event(kind: EventKind) -> SessionEvent {
        SessionEvent {
            timestamp: Utc::now(),
            kind,
        }
    }

    #[test]
    fn test_theme_construction() {
        let theme = Theme::new();
        // border_focused has no DIM modifier
        assert!(!theme.border_focused.add_modifier.contains(Modifier::DIM));
        // border_unfocused has DIM modifier
        assert!(theme.border_unfocused.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn test_icon_set_unicode_preset() {
        let icons = IconSet::from_config(IconConfig::default());
        assert_eq!(icons.diag_error, "\u{2717} ");
    }

    #[test]
    fn test_icon_set_nerd_preset() {
        let config = IconConfig {
            preset: IconPreset::Nerd,
            ..IconConfig::default()
        };
        let icons = IconSet::from_config(config);
        assert_eq!(icons.diag_error, " ");
    }

    #[test]
    fn test_icon_set_overrides() {
        let config = IconConfig {
            lock: Some("\u{1F512} ".into()),
            ..IconConfig::default()
        };
        let icons = IconSet::from_config(config);
        assert_eq!(icons.lock, "\u{1F512} ");
    }

    #[test]
    fn test_format_event_plain_tool_call() {
        let ev = make_event(EventKind::ToolCall {
            tool: "hover".to_string(),
            file: Some("/src/main.rs".to_string()),
        });
        let plain = format_event_plain(&ev);
        assert!(plain.contains("hover"));
    }

    #[test]
    fn test_format_event_plain_diagnostics() {
        let ev = make_event(EventKind::Diagnostics {
            file: "/src/lib.rs".to_string(),
            count: 3,
            preview: "[error] something".to_string(),
        });
        let plain = format_event_plain(&ev);
        assert!(plain.contains("lib.rs"));
        assert!(plain.contains('3'));
    }

    #[test]
    fn test_collapse_progress_consecutive() {
        let ev1 = make_event(EventKind::Progress {
            language: "rust".to_string(),
            title: "Indexing".to_string(),
            message: Some("1/10".to_string()),
            percentage: Some(10),
        });
        let ev2 = make_event(EventKind::Progress {
            language: "rust".to_string(),
            title: "Indexing".to_string(),
            message: Some("5/10".to_string()),
            percentage: Some(50),
        });
        let ev3 = make_event(EventKind::Progress {
            language: "rust".to_string(),
            title: "Indexing".to_string(),
            message: Some("10/10".to_string()),
            percentage: Some(100),
        });
        let interrupt = make_event(EventKind::Started);
        let ev4 = make_event(EventKind::Progress {
            language: "rust".to_string(),
            title: "Indexing".to_string(),
            message: Some("1/5".to_string()),
            percentage: Some(20),
        });

        // Three consecutive same-key progress events collapse to one.
        let collapsed = collapse_progress(vec![&ev1, &ev2, &ev3]);
        assert_eq!(collapsed.len(), 1);

        // Intersperse a non-progress event — the run resets.
        let collapsed = collapse_progress(vec![&ev1, &ev2, &interrupt, &ev3, &ev4]);
        assert_eq!(collapsed.len(), 3); // ev2, interrupt, ev4
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
}

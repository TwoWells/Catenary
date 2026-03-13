// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Messages panel: renders a list of protocol messages with cursor, scroll
//! offset, tail attach/detach behavior, and horizontal scroll indicators.
//!
//! This is the core building block — later tickets add expansion (04),
//! multi-panel grid (05), scrollbar (06), and selection (07) on top of this.

use std::collections::{HashMap, HashSet};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Widget};
use unicode_width::UnicodeWidthStr;

use super::selection::VisualSelection;
use super::theme::{IconSet, Theme, format_message_plain, format_message_styled};
use crate::session::SessionMessage;

// ── Data types ──────────────────────────────────────────────────────────

/// Language server lifecycle state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LsState {
    /// Server has not been loaded yet.
    NotLoaded,
    /// Server is initializing.
    Initializing,
    /// Server is sending progress notifications.
    Progress,
    /// Server is healthy and running.
    Healthy,
    /// Server has crashed.
    Crashed,
}

/// Language server status shown in the panel title.
#[derive(Clone, Debug)]
pub struct LanguageServerStatus {
    /// Language server name (e.g., "rust", "ts").
    pub name: String,
    /// Current lifecycle state.
    pub state: LsState,
}

/// A line in the flattened view — either a message header or a detail line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlatLine {
    /// A message header (the one-line summary).
    MessageHeader {
        /// Index into the messages vec.
        message_index: usize,
    },
    /// A detail line within an expanded message.
    Detail {
        /// Index into the messages vec.
        message_index: usize,
        /// Index of this detail line within the expansion.
        detail_index: usize,
    },
}

/// State for a single messages panel.
pub struct PanelState<'a> {
    /// Session ID this panel is tailing.
    pub session_id: String,
    /// All messages loaded for this session.
    pub messages: Vec<SessionMessage>,
    /// Cursor position (index into flat lines).
    pub cursor: usize,
    /// Scroll offset from top of content.
    pub scroll_offset: usize,
    /// Whether the panel is attached to the tail (auto-scrolling).
    pub tail_attached: bool,
    /// Horizontal scroll offset (for wide lines).
    pub horizontal_scroll: usize,
    /// Whether this panel is pinned (enlarged).
    pub pinned: bool,
    /// Language server statuses for the title bar.
    pub language_servers: Vec<LanguageServerStatus>,
    /// Indices of expanded messages (in the messages Vec).
    pub expanded: HashSet<usize>,
    /// Active visual selection, if any.
    pub visual_selection: Option<VisualSelection>,
    /// Last known viewport height (updated each render frame).
    pub viewport_height: usize,
    /// Display ID for the title bar (client session ID if available, else internal ID).
    pub display_id: String,
    /// Active filter pattern (case-insensitive substring match).
    pub filter_pattern: Option<String>,
    /// Semantic color theme (borrowed from the application).
    pub theme: &'a Theme,
    /// Resolved icon set (borrowed from the application).
    pub icons: &'a IconSet,
}

// ── Construction & navigation ───────────────────────────────────────────

impl<'a> PanelState<'a> {
    /// Create a new panel for the given session.
    ///
    /// Starts with empty messages, cursor at 0, tail attached, no horizontal
    /// scroll, not pinned.
    #[must_use]
    pub fn new(session_id: String, theme: &'a Theme, icons: &'a IconSet) -> Self {
        let display_id = session_id.clone();
        Self {
            session_id,
            messages: Vec::new(),
            cursor: 0,
            scroll_offset: 0,
            tail_attached: true,
            horizontal_scroll: 0,
            pinned: false,
            language_servers: Vec::new(),
            expanded: HashSet::new(),
            display_id,
            visual_selection: None,
            viewport_height: 0,
            filter_pattern: None,
            theme,
            icons,
        }
    }

    /// Total number of visible lines (flat lines including expanded detail).
    fn total_lines(&self) -> usize {
        self.flat_lines().len()
    }

    /// Load historical messages. Sets cursor to the last line and attaches tail.
    pub fn load_messages(&mut self, messages: Vec<SessionMessage>) {
        self.messages = messages;
        self.expanded.clear();
        let total = self.total_lines();
        self.cursor = total.saturating_sub(1);
        self.tail_attached = true;
        self.snap_viewport(0);
    }

    /// Append a new message.
    ///
    /// If tail attached, advance cursor and scroll to keep the latest message
    /// visible. If detached, just append (cursor stays put).
    pub fn push_message(&mut self, msg: SessionMessage) {
        self.messages.push(msg);
        if self.tail_attached {
            let total = self.total_lines();
            self.cursor = total.saturating_sub(1);
            self.snap_viewport(0);
        }
    }

    /// Move cursor by `delta`. Clamp to `[0, total_lines - 1]`.
    ///
    /// - Moving up (`delta < 0`): detach tail.
    /// - Moving down past the last line: reattach tail, cursor on last.
    /// - Snap viewport to center cursor (`scrolloff=999` behavior).
    #[allow(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        reason = "terminal item counts never overflow isize"
    )]
    pub fn navigate(&mut self, delta: isize) {
        let total = self.total_lines();
        if total == 0 {
            self.cursor = 0;
            return;
        }

        let max = (total - 1) as isize;
        let new_pos = self.cursor as isize + delta;

        if delta < 0 {
            self.tail_attached = false;
        }

        if new_pos > max {
            // Moved past end — reattach.
            self.cursor = total - 1;
            self.tail_attached = true;
        } else {
            self.cursor = new_pos.max(0) as usize;
        }

        self.snap_viewport(0);
    }

    /// Scroll viewport by `delta` lines without moving the cursor.
    ///
    /// Used for mouse wheel: moves `scroll_offset` only, detaches tail on
    /// scroll-up, reattaches when scrolled to the very bottom.
    #[allow(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        reason = "terminal item counts never overflow isize"
    )]
    pub fn scroll_viewport(&mut self, delta: isize) {
        let total = self.total_lines();
        if total == 0 {
            return;
        }

        if delta < 0 {
            self.tail_attached = false;
        }

        let new_offset = (self.scroll_offset as isize + delta)
            .max(0)
            .min(total.saturating_sub(1) as isize);

        #[allow(clippy::cast_sign_loss, reason = "clamped to >= 0")]
        {
            self.scroll_offset = new_offset as usize;
        }

        // Reattach tail if scrolled to the very bottom.
        let vh = if self.viewport_height > 0 {
            self.viewport_height
        } else {
            20
        };
        if self.scroll_offset + vh >= total {
            self.tail_attached = true;
        }
    }

    /// Jump to first line — `g` key.
    pub const fn scroll_to_top(&mut self) {
        self.cursor = 0;
        self.scroll_offset = 0;
        self.tail_attached = false;
    }

    /// Jump to last line — `G` key.
    pub fn scroll_to_bottom(&mut self) {
        let total = self.total_lines();
        self.cursor = total.saturating_sub(1);
        self.tail_attached = true;
        self.snap_viewport(0);
    }

    /// Page up — `Ctrl+U`.
    pub fn page_up(&mut self, height: usize) {
        let half = (height / 2).max(1);
        #[allow(
            clippy::cast_possible_wrap,
            reason = "terminal heights never overflow isize"
        )]
        self.navigate(-(half as isize));
    }

    /// Page down — `Ctrl+D`.
    pub fn page_down(&mut self, height: usize) {
        let half = (height / 2).max(1);
        #[allow(
            clippy::cast_possible_wrap,
            reason = "terminal heights never overflow isize"
        )]
        self.navigate(half as isize);
    }

    /// Compute the `(start, end)` indices of lines visible in the viewport.
    ///
    /// `height` is the inner content height (excluding title bar and borders).
    #[must_use]
    pub fn visible_range(&self, height: usize) -> (usize, usize) {
        let total = self.total_lines();
        let start = self.scroll_offset.min(total);
        let end = (start + height).min(total);
        (start, end)
    }

    /// Derive language server statuses from messages.
    ///
    /// Scans for LSP messages and tracks unique server names. Any server
    /// with messages is considered healthy.
    pub fn update_language_servers(&mut self) {
        let mut seen: HashSet<String> = HashSet::new();
        let mut order: Vec<String> = Vec::new();

        for msg in &self.messages {
            if msg.r#type == "lsp" && !msg.server.is_empty() && seen.insert(msg.server.clone()) {
                order.push(msg.server.clone());
            }
        }

        self.language_servers = order
            .into_iter()
            .map(|name| LanguageServerStatus {
                name,
                state: LsState::Healthy,
            })
            .collect();
    }

    /// Snap viewport so cursor is centered (`scrolloff=999` behavior).
    ///
    /// `height` of 0 means use the last known viewport height from the
    /// previous render frame. Falls back to 20 if never rendered.
    fn snap_viewport(&mut self, height: usize) {
        let h = if height > 0 {
            height
        } else if self.viewport_height > 0 {
            self.viewport_height
        } else {
            20
        };
        let total = self.total_lines();
        if total <= h {
            self.scroll_offset = 0;
            return;
        }
        let target = self.cursor.saturating_sub(h / 2);
        self.scroll_offset = target.min(total.saturating_sub(h));
    }

    // ── Expansion ───────────────────────────────────────────────────────

    /// Build a flat list of lines: message headers interleaved with detail
    /// lines for any expanded messages.
    #[must_use]
    pub fn flat_lines(&self) -> Vec<FlatLine> {
        let lower_pattern = self.filter_pattern.as_ref().map(|p| p.to_lowercase());
        let mut lines = Vec::new();
        for (message_index, msg) in self.messages.iter().enumerate() {
            if let Some(ref pat) = lower_pattern {
                let plain = format_message_plain(msg);
                if !plain.to_lowercase().contains(pat) {
                    continue;
                }
            }
            lines.push(FlatLine::MessageHeader { message_index });
            if self.expanded.contains(&message_index) {
                let count = detail_lines(msg, self.theme).len();
                for detail_index in 0..count {
                    lines.push(FlatLine::Detail {
                        message_index,
                        detail_index,
                    });
                }
            }
        }
        lines
    }

    /// Quick check: does this message have expandable detail?
    #[must_use]
    pub fn has_detail(&self, message_index: usize) -> bool {
        self.messages
            .get(message_index)
            .is_some_and(|msg| msg.payload.as_object().is_some_and(|o| !o.is_empty()))
    }

    /// Toggle expansion of the message under the cursor.
    ///
    /// - On a `MessageHeader`: toggle the message in/out of `expanded`.
    /// - On a `Detail` line: collapse the parent message, move cursor to its header.
    /// - On a message with no detail: no-op.
    pub fn toggle_expansion(&mut self) {
        let flat = self.flat_lines();
        let Some(current) = flat.get(self.cursor) else {
            return;
        };
        match *current {
            FlatLine::MessageHeader { message_index } => {
                if !self.has_detail(message_index) {
                    return;
                }
                if self.expanded.contains(&message_index) {
                    self.expanded.remove(&message_index);
                } else {
                    self.expanded.insert(message_index);
                }
            }
            FlatLine::Detail { message_index, .. } => {
                self.expanded.remove(&message_index);
                // Move cursor to the parent header.
                let new_flat = self.flat_lines();
                if let Some(pos) = new_flat.iter().position(|fl| {
                    matches!(fl, FlatLine::MessageHeader { message_index: mi } if *mi == message_index)
                }) {
                    self.cursor = pos;
                }
            }
        }
        self.snap_viewport(0);
    }
}

// ── Expansion helpers ───────────────────────────────────────────────────

/// Generate styled detail lines for an expanded message.
///
/// Returns an empty vec for messages with empty payloads.
#[must_use]
pub fn detail_lines(msg: &SessionMessage, theme: &Theme) -> Vec<Line<'static>> {
    let payload = &msg.payload;
    if payload.as_object().is_none_or(serde_json::Map::is_empty) {
        return Vec::new();
    }

    // Indent to align past the timestamp column ("HH:MM:SS  " = 10 chars).
    let indent = "          ";
    let mut lines = Vec::new();

    // Line 1: method [type]
    lines.push(Line::from(vec![
        Span::raw(indent.to_string()),
        Span::styled(format!("{} [{}]", msg.method, msg.r#type), theme.muted),
    ]));

    // Line 2: separator
    lines.push(Line::from(vec![
        Span::raw(indent.to_string()),
        Span::styled("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}", theme.muted),
    ]));

    // Lines 3+: pretty-printed payload
    if let Ok(pretty) = serde_json::to_string_pretty(payload) {
        for line in pretty.lines() {
            lines.push(Line::from(vec![
                Span::raw(indent.to_string()),
                Span::styled(line.to_string(), theme.muted),
            ]));
        }
    }

    lines
}

// ── Rendering ───────────────────────────────────────────────────────────

/// Style for a language server status icon.
fn ls_status_style(state: &LsState) -> Style {
    match state {
        LsState::NotLoaded => Style::default().fg(Color::White),
        LsState::Initializing => Style::default().fg(Color::Yellow),
        LsState::Progress => Style::default().fg(Color::Cyan),
        LsState::Healthy => Style::default().fg(Color::Green),
        LsState::Crashed => Style::default().fg(Color::Red),
    }
}

/// Status icon for a language server state, resolved from the icon set.
fn ls_status_icon<'a>(state: &LsState, icons: &'a IconSet) -> &'a str {
    match state {
        LsState::NotLoaded => &icons.ls_inactive,
        _ => &icons.ls_active,
    }
}

/// Build the title line for a panel.
fn build_title(state: &PanelState<'_>) -> Line<'static> {
    let id_short = if state.display_id.len() > 8 {
        &state.display_id[..8]
    } else {
        &state.display_id
    };

    let mut spans = vec![Span::raw(format!(" Events [{id_short}]"))];

    if state.language_servers.is_empty() {
        spans.push(Span::styled(" no ls", Style::default().fg(Color::DarkGray)));
    } else {
        spans.push(Span::raw(" "));
        for (i, ls) in state.language_servers.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" \u{2571} ")); // ╱
            }
            let style = ls_status_style(&ls.state);
            spans.push(Span::styled(
                ls_status_icon(&ls.state, state.icons).to_string(),
                style,
            ));
            spans.push(Span::styled(ls.name.clone(), style));
        }
    }

    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// Convert a borrowed `Line` into a fully owned `Line<'static>`.
#[must_use]
pub fn to_owned_line(line: &Line<'_>) -> Line<'static> {
    Line::from(
        line.spans
            .iter()
            .map(|s| Span::styled(s.content.to_string(), s.style))
            .collect::<Vec<_>>(),
    )
}

/// Apply horizontal scrolling to a styled line, inserting clip indicators.
///
/// Returns a new line clipped to `width` display columns, with `◀…` on the
/// left when content is clipped left and `…▶` on the right when clipped right.
fn clip_line_horizontal(line: &Line<'_>, h_scroll: usize, width: usize) -> Line<'static> {
    if width < 4 {
        // Too narrow for indicators, just return empty.
        return Line::default();
    }

    // Flatten the line into a single plain string + collect grapheme-aware info.
    let full_text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let full_width = UnicodeWidthStr::width(full_text.as_str());

    if full_width == 0 {
        return Line::default();
    }

    // No clipping needed at all.
    if h_scroll == 0 && full_width <= width {
        return to_owned_line(line);
    }

    let clipped_left = h_scroll > 0;
    let clipped_right = full_width > h_scroll + width;

    // Calculate available content width after reserving indicator space.
    let left_reserve = if clipped_left { 2 } else { 0 };
    let right_reserve = if clipped_right { 2 } else { 0 };
    let content_width = width.saturating_sub(left_reserve + right_reserve);

    if content_width == 0 {
        let mut spans = Vec::new();
        if clipped_left {
            spans.push(Span::styled(
                "\u{25C0}\u{2026}",
                Style::default().fg(Color::DarkGray),
            ));
        }
        if clipped_right {
            spans.push(Span::styled(
                "\u{2026}\u{25B6}",
                Style::default().fg(Color::DarkGray),
            ));
        }
        return Line::from(spans);
    }

    // Walk through spans, tracking display-width position, and extract the
    // visible portion respecting h_scroll and content_width.
    let vis_start = h_scroll;
    let vis_end = h_scroll + content_width;

    let mut result_spans: Vec<Span<'static>> = Vec::new();

    if clipped_left {
        result_spans.push(Span::styled(
            "\u{25C0}\u{2026}",
            Style::default().fg(Color::DarkGray),
        )); // ◀…
    }

    // Walk spans and extract visible portion.
    let mut col = 0usize;
    for span in &line.spans {
        let span_text = span.content.as_ref();
        let span_width = UnicodeWidthStr::width(span_text);
        let span_end = col + span_width;

        if span_end <= vis_start || col >= vis_end {
            // Entirely outside visible range.
            col = span_end;
            continue;
        }

        // Partially or fully visible. Extract the visible chars.
        let mut visible = String::new();
        let mut char_col = col;
        for ch in span_text.chars() {
            let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            let ch_end = char_col + ch_width;
            if char_col >= vis_end {
                break;
            }
            if ch_end > vis_start {
                visible.push(ch);
            }
            char_col = ch_end;
        }

        if !visible.is_empty() {
            result_spans.push(Span::styled(visible, span.style));
        }

        col = span_end;
    }

    if clipped_right {
        result_spans.push(Span::styled(
            "\u{2026}\u{25B6}",
            Style::default().fg(Color::DarkGray),
        )); // …▶
    }

    Line::from(result_spans)
}

/// Render a single messages panel into the given buffer area.
///
/// The panel owns its top row (title bar) and right column (scrollbar,
/// rendered by ticket 06). Left and bottom edges are content. The caller
/// (grid) handles junction characters.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
pub fn render_panel(state: &PanelState<'_>, area: Rect, buf: &mut Buffer, focused: bool) {
    if area.width < 4 || area.height < 2 {
        return;
    }

    let border_style = if focused {
        state.theme.border_focused
    } else {
        state.theme.border_unfocused
    };

    let title_style = if focused {
        state.theme.title
    } else {
        state.theme.border_unfocused
    };

    let border_set = if focused {
        symbols::border::THICK
    } else {
        symbols::border::PLAIN
    };

    let title = build_title(state);
    let block = Block::default()
        .borders(Borders::TOP | Borders::RIGHT)
        .border_set(border_set)
        .border_style(border_style)
        .title(title)
        .title_style(title_style);
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.width < 2 || inner.height < 1 {
        return;
    }

    // Build flat line list (headers + expanded detail lines).
    let flat = state.flat_lines();

    // Viewport slicing.
    let height = inner.height as usize;
    let total = flat.len();
    let start = state.scroll_offset.min(total);
    let end = (start + height).min(total);

    // Cache detail lines per expanded message to avoid recomputation.
    let mut detail_cache: HashMap<usize, Vec<Line<'static>>> = HashMap::new();

    // Render each visible line.
    let content_width = inner.width as usize;
    for (i, fl) in flat[start..end].iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }

        let line = match fl {
            FlatLine::MessageHeader { message_index } => {
                format_message_styled(&state.messages[*message_index], state.icons, state.theme)
            }
            FlatLine::Detail {
                message_index,
                detail_index,
            } => detail_cache
                .entry(*message_index)
                .or_insert_with(|| detail_lines(&state.messages[*message_index], state.theme))
                .get(*detail_index)
                .cloned()
                .unwrap_or_default(),
        };

        let display_line = if state.horizontal_scroll > 0
            || UnicodeWidthStr::width(
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .as_str(),
            ) > content_width
        {
            clip_line_horizontal(&line, state.horizontal_scroll, content_width)
        } else {
            to_owned_line(&line)
        };

        // Apply cursor highlight to the entire row.
        let line_index = start + i;
        if line_index == state.cursor {
            // Set selection style on the entire row first.
            for x in inner.x..inner.x + inner.width {
                buf[(x, y)].set_style(state.theme.selection);
            }
        }

        buf.set_line(inner.x, y, &display_line, inner.width);

        // Re-apply selection style on top of content for cursor row.
        if line_index == state.cursor {
            for x in inner.x..inner.x + inner.width {
                buf[(x, y)].set_style(state.theme.selection);
            }
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::config::IconConfig;
    use crate::session::SessionMessage;

    fn test_theme() -> Theme {
        Theme::new()
    }

    fn test_icons() -> IconSet {
        IconSet::from_config(IconConfig::default())
    }

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

    /// An LSP message with a non-empty payload (expandable).
    fn make_lsp_message() -> SessionMessage {
        make_message_with_payload(
            "lsp",
            "textDocument/hover",
            "rust-analyzer",
            serde_json::json!({"id": 1, "method": "textDocument/hover", "params": {}}),
        )
    }

    /// A hook diagnostic message with preview (expandable).
    fn make_hook_diag_message(file: &str, count: u64) -> SessionMessage {
        make_message_with_payload(
            "hook",
            "post-tool",
            "catenary",
            serde_json::json!({
                "file": file,
                "count": count,
                "preview": "\t:12:1 [error] rustc: something"
            }),
        )
    }

    /// Convert a ratatui buffer to a single string for assertion matching.
    fn buffer_to_string(buf: &Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                s.push_str(cell.symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn test_panel_new_tail_attached() {
        let theme = test_theme();
        let icons = test_icons();
        let panel = PanelState::new("abc123".to_string(), &theme, &icons);
        assert!(panel.tail_attached);
        assert_eq!(panel.cursor, 0);
        assert_eq!(panel.scroll_offset, 0);
        assert_eq!(panel.horizontal_scroll, 0);
        assert!(!panel.pinned);
        assert!(panel.messages.is_empty());
    }

    #[test]
    fn test_panel_load_messages() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages: Vec<SessionMessage> = (0..10)
            .map(|_| make_message("lsp", "initialized", "rust-analyzer"))
            .collect();
        panel.load_messages(messages);
        assert_eq!(panel.messages.len(), 10);
        assert_eq!(panel.cursor, 9);
        assert!(panel.tail_attached);
    }

    #[test]
    fn test_panel_push_message_attached() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages: Vec<SessionMessage> = (0..5)
            .map(|_| make_message("lsp", "initialized", "rust-analyzer"))
            .collect();
        panel.load_messages(messages);
        assert_eq!(panel.cursor, 4);

        panel.push_message(make_message("mcp", "tools/list", "catenary"));
        assert_eq!(panel.messages.len(), 6);
        assert_eq!(panel.cursor, 5);
        assert!(panel.tail_attached);
    }

    #[test]
    fn test_panel_push_message_detached() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages: Vec<SessionMessage> = (0..5)
            .map(|_| make_message("lsp", "initialized", "rust-analyzer"))
            .collect();
        panel.load_messages(messages);

        // Navigate up to detach.
        panel.navigate(-1);
        assert!(!panel.tail_attached);
        let cursor_before = panel.cursor;

        panel.push_message(make_message("mcp", "tools/list", "catenary"));
        assert_eq!(panel.messages.len(), 6);
        assert_eq!(panel.cursor, cursor_before);
        assert!(!panel.tail_attached);
    }

    #[test]
    fn test_panel_navigate_up_detaches() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages: Vec<SessionMessage> = (0..10)
            .map(|_| make_message("lsp", "initialized", "rust-analyzer"))
            .collect();
        panel.load_messages(messages);
        assert_eq!(panel.cursor, 9);
        assert!(panel.tail_attached);

        panel.navigate(-1);
        assert_eq!(panel.cursor, 8);
        assert!(!panel.tail_attached);
    }

    #[test]
    fn test_panel_navigate_down_past_end_reattaches() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages: Vec<SessionMessage> = (0..5)
            .map(|_| make_message("lsp", "initialized", "rust-analyzer"))
            .collect();
        panel.load_messages(messages);

        // Navigate up to detach.
        panel.navigate(-2);
        assert_eq!(panel.cursor, 2);
        assert!(!panel.tail_attached);

        // Navigate down past the end.
        panel.navigate(1);
        assert_eq!(panel.cursor, 3);
        panel.navigate(1);
        assert_eq!(panel.cursor, 4);
        // One more should clamp and reattach.
        panel.navigate(1);
        assert_eq!(panel.cursor, 4);
        assert!(panel.tail_attached);
    }

    #[test]
    fn test_panel_scroll_to_top() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages: Vec<SessionMessage> = (0..20)
            .map(|_| make_message("lsp", "initialized", "rust-analyzer"))
            .collect();
        panel.load_messages(messages);

        panel.scroll_to_top();
        assert_eq!(panel.cursor, 0);
        assert_eq!(panel.scroll_offset, 0);
        assert!(!panel.tail_attached);
    }

    #[test]
    fn test_panel_scroll_to_bottom() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages: Vec<SessionMessage> = (0..20)
            .map(|_| make_message("lsp", "initialized", "rust-analyzer"))
            .collect();
        panel.load_messages(messages);

        panel.scroll_to_top();
        assert!(!panel.tail_attached);

        panel.scroll_to_bottom();
        assert_eq!(panel.cursor, 19);
        assert!(panel.tail_attached);
    }

    #[test]
    fn test_panel_visible_range() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages: Vec<SessionMessage> = (0..100)
            .map(|_| make_message("lsp", "initialized", "rust-analyzer"))
            .collect();
        panel.load_messages(messages);

        // Move cursor to 50 and snap viewport.
        panel.cursor = 50;
        panel.snap_viewport(20);

        let (start, end) = panel.visible_range(20);
        // Cursor at 50, centered in height 20 → offset ~40.
        assert_eq!(start, 40);
        assert_eq!(end, 60);
    }

    #[test]
    fn test_panel_visible_range_at_top() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages: Vec<SessionMessage> = (0..100)
            .map(|_| make_message("lsp", "initialized", "rust-analyzer"))
            .collect();
        panel.load_messages(messages);

        // Cursor near top — can't center.
        panel.cursor = 3;
        panel.snap_viewport(20);

        let (start, end) = panel.visible_range(20);
        assert_eq!(start, 0);
        assert_eq!(end, 20);
    }

    #[test]
    fn test_panel_visible_range_at_bottom() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages: Vec<SessionMessage> = (0..100)
            .map(|_| make_message("lsp", "initialized", "rust-analyzer"))
            .collect();
        panel.load_messages(messages);

        // Cursor near bottom.
        panel.cursor = 97;
        panel.snap_viewport(20);

        let (start, end) = panel.visible_range(20);
        assert_eq!(end, 100);
        assert_eq!(start, 80);
    }

    #[test]
    fn test_panel_render_messages() {
        let theme = test_theme();
        let icons = test_icons();
        let messages: Vec<SessionMessage> = vec![
            make_message_with_payload(
                "mcp",
                "tools/call",
                "catenary",
                serde_json::json!({"params": {"name": "grep"}}),
            ),
            make_message_with_payload(
                "mcp",
                "tools/call",
                "catenary",
                serde_json::json!({"params": {"name": "glob"}}),
            ),
        ];

        let mut panel = PanelState::new("test1234".to_string(), &theme, &icons);
        panel.load_messages(messages);

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_panel(&panel, area, f.buffer_mut(), true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(content.contains("grep"), "expected grep tool name");
        assert!(content.contains("glob"), "expected glob tool name");
    }

    #[test]
    fn test_panel_render_empty() {
        let theme = test_theme();
        let icons = test_icons();
        let panel = PanelState::new("empty123".to_string(), &theme, &icons);

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_panel(&panel, area, f.buffer_mut(), true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        // Should contain the title, no panic.
        assert!(content.contains("Events"), "expected title in empty panel");
    }

    #[test]
    fn test_panel_render_cursor_highlight() {
        let theme = test_theme();
        let icons = test_icons();
        let messages: Vec<SessionMessage> = (0..5)
            .map(|_| {
                make_message_with_payload(
                    "mcp",
                    "tools/call",
                    "catenary",
                    serde_json::json!({"params": {"name": "grep"}}),
                )
            })
            .collect();

        let mut panel = PanelState::new("test1234".to_string(), &theme, &icons);
        panel.load_messages(messages);
        // Set cursor to row 1 (second message in visible area).
        panel.cursor = 1;
        panel.snap_viewport(8);

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_panel(&panel, area, f.buffer_mut(), true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();

        // The cursor row (row 1 in content = y=2 in buffer since y=0 is border,
        // y=1 is first content row, y=2 is second content row).
        // With cursor at index 1 and scroll_offset 0, cursor is at visible row 1.
        // Inner area starts at y=1 (after top border), so cursor row is at y=2.
        let cursor_y = 2u16;
        let inner_x = 1u16; // after left border
        let cell = &buf[(inner_x, cursor_y)];
        // The selection style uses REVERSED modifier.
        assert!(
            cell.modifier.contains(ratatui::style::Modifier::REVERSED),
            "expected REVERSED modifier on cursor row"
        );
    }

    #[test]
    fn test_panel_language_server_status() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        panel.messages = vec![
            make_message("lsp", "textDocument/hover", "rust-analyzer"),
            make_message(
                "lsp",
                "textDocument/completion",
                "typescript-language-server",
            ),
        ];

        panel.update_language_servers();
        assert_eq!(panel.language_servers.len(), 2);
        assert_eq!(panel.language_servers[0].name, "rust-analyzer");
        assert_eq!(panel.language_servers[0].state, LsState::Healthy);
        assert_eq!(panel.language_servers[1].name, "typescript-language-server");
        assert_eq!(panel.language_servers[1].state, LsState::Healthy);
    }

    // ── Expansion tests ─────────────────────────────────────────────────

    #[test]
    fn test_flat_lines_no_expansion() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let messages: Vec<SessionMessage> = (0..5)
            .map(|_| make_message("lsp", "initialized", "rust-analyzer"))
            .collect();
        panel.load_messages(messages);

        let flat = panel.flat_lines();
        assert_eq!(flat.len(), 5);
        for (i, fl) in flat.iter().enumerate() {
            assert_eq!(*fl, FlatLine::MessageHeader { message_index: i });
        }
    }

    #[test]
    fn test_flat_lines_one_expanded() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let messages = vec![
            make_message("lsp", "initialized", "rust-analyzer"),
            make_hook_diag_message("/src/lib.rs", 3),
            make_message("mcp", "tools/list", "catenary"),
        ];
        panel.load_messages(messages);
        panel.expanded.insert(1);

        let flat = panel.flat_lines();
        // 3 headers + detail lines for the expanded hook message
        let detail_count = detail_lines(&panel.messages[1], &theme).len();
        assert!(detail_count > 0, "hook diag message should have details");
        assert_eq!(flat.len(), 3 + detail_count);
        assert_eq!(flat[0], FlatLine::MessageHeader { message_index: 0 });
        assert_eq!(flat[1], FlatLine::MessageHeader { message_index: 1 });
        for i in 0..detail_count {
            assert_eq!(
                flat[2 + i],
                FlatLine::Detail {
                    message_index: 1,
                    detail_index: i
                }
            );
        }
        assert_eq!(
            flat[2 + detail_count],
            FlatLine::MessageHeader { message_index: 2 }
        );
    }

    #[test]
    fn test_toggle_expansion_header() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let messages = vec![
            make_message("lsp", "initialized", "rust-analyzer"),
            make_lsp_message(),
        ];
        panel.load_messages(messages);
        // Cursor on message 1 (the expandable LSP message).
        panel.cursor = 1;

        panel.toggle_expansion();
        assert!(panel.expanded.contains(&1));

        panel.toggle_expansion();
        assert!(!panel.expanded.contains(&1));
    }

    #[test]
    fn test_toggle_expansion_detail() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let messages = vec![
            make_message("lsp", "initialized", "rust-analyzer"),
            make_lsp_message(),
            make_message("mcp", "tools/list", "catenary"),
        ];
        panel.load_messages(messages);
        panel.expanded.insert(1);
        // Find a detail line index.
        let flat = panel.flat_lines();
        let detail_pos = flat
            .iter()
            .position(|fl| {
                matches!(
                    fl,
                    FlatLine::Detail {
                        message_index: 1,
                        ..
                    }
                )
            })
            .expect("should have detail lines");
        panel.cursor = detail_pos;

        panel.toggle_expansion();
        assert!(!panel.expanded.contains(&1));
        // After collapse: cursor should be on message 1's header.
        assert_eq!(panel.cursor, 1);
    }

    #[test]
    fn test_toggle_expansion_no_detail() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        // Empty payload → no detail
        let messages = vec![make_message("lsp", "initialized", "rust-analyzer")];
        panel.load_messages(messages);
        panel.cursor = 0;

        panel.toggle_expansion();
        assert!(panel.expanded.is_empty());
    }

    #[test]
    fn test_cursor_walks_detail_lines() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let messages = vec![
            make_message("lsp", "initialized", "rust-analyzer"),
            make_lsp_message(),
            make_message("mcp", "tools/list", "catenary"),
        ];
        panel.load_messages(messages);
        panel.expanded.insert(1);

        let flat = panel.flat_lines();
        panel.cursor = 0;
        panel.tail_attached = false;

        // Walk through all lines one by one.
        for expected in flat.iter().skip(1) {
            panel.navigate(1);
            let current_flat = panel.flat_lines();
            assert_eq!(current_flat[panel.cursor], *expected);
        }
    }

    #[test]
    fn test_detail_lines_non_empty_payload() {
        let msg = make_lsp_message();
        let theme = test_theme();

        let lines = detail_lines(&msg, &theme);
        // Should have: method [type] header + separator + payload lines
        assert!(lines.len() >= 3, "should have header + sep + payload");
        let hdr: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            hdr.contains("textDocument/hover"),
            "header should contain method"
        );
        assert!(hdr.contains("[lsp]"), "header should contain type");
        let sep: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(sep.contains("\u{2500}"), "second line should be separator");
    }

    #[test]
    fn test_detail_lines_empty_payload() {
        let msg = make_message("lsp", "initialized", "rust-analyzer");
        let theme = test_theme();

        let lines = detail_lines(&msg, &theme);
        assert!(
            lines.is_empty(),
            "empty payload should have no detail lines"
        );
    }

    #[test]
    fn test_visible_range_with_expansion() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let mut messages: Vec<SessionMessage> = (0..10)
            .map(|_| make_message("lsp", "initialized", "rust-analyzer"))
            .collect();
        // Replace message 5 with an expandable one.
        messages[5] = make_lsp_message();
        panel.load_messages(messages);
        panel.expanded.insert(5);

        let flat = panel.flat_lines();
        let detail_count = detail_lines(&panel.messages[5], &theme).len();
        // 10 headers + detail lines for message 5
        assert_eq!(flat.len(), 10 + detail_count);

        // Set cursor to 0, snap viewport.
        panel.cursor = 0;
        panel.snap_viewport(10);
        let (start, end) = panel.visible_range(10);
        assert_eq!(start, 0);
        assert_eq!(end, 10);
    }

    #[test]
    fn test_render_expanded_message() {
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![make_hook_diag_message("/src/lib.rs", 2)];

        let mut panel = PanelState::new("test1234".to_string(), &theme, &icons);
        panel.load_messages(messages);
        panel.expanded.insert(0);
        panel.cursor = 0;
        panel.snap_viewport(8);

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_panel(&panel, area, f.buffer_mut(), true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        // Header should show the diagnostics summary.
        assert!(content.contains("lib.rs"), "expected file name in header");
        // Detail lines should contain the payload.
        assert!(content.contains("post-tool"), "expected method in detail");
    }

    #[test]
    fn test_has_detail_non_empty_payload() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.messages = vec![
            make_lsp_message(),
            make_message("lsp", "initialized", "rust-analyzer"),
        ];
        assert!(panel.has_detail(0), "non-empty payload should have detail");
        assert!(!panel.has_detail(1), "empty payload should not have detail");
    }
}

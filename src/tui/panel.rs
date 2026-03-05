// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Events panel: renders a list of session events with cursor, scroll offset,
//! tail attach/detach behavior, and horizontal scroll indicators.
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
use super::theme::{IconSet, Theme, diag_style, format_event_styled};
use crate::session::{EventKind, SessionEvent};

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

/// A line in the flattened view — either an event header or a detail line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlatLine {
    /// An event header (the one-line summary).
    EventHeader {
        /// Index into the events vec.
        event_index: usize,
    },
    /// A detail line within an expanded event.
    Detail {
        /// Index into the events vec.
        event_index: usize,
        /// Index of this detail line within the expansion.
        detail_index: usize,
    },
}

/// State for a single Events panel.
pub struct PanelState<'a> {
    /// Session ID this panel is tailing.
    pub session_id: String,
    /// All events loaded for this session.
    pub events: Vec<SessionEvent>,
    /// Cursor position (index into events after collapse).
    pub cursor: usize,
    /// Scroll offset from top of content.
    pub scroll_offset: usize,
    /// Whether the panel is attached to the tail (auto-scrolling).
    pub tail_attached: bool,
    /// Horizontal scroll offset (for wide event lines).
    pub horizontal_scroll: usize,
    /// Whether this panel is pinned (enlarged).
    pub pinned: bool,
    /// Language server statuses for the title bar.
    pub language_servers: Vec<LanguageServerStatus>,
    /// Indices of expanded events (in the events Vec).
    pub expanded: HashSet<usize>,
    /// Active visual selection, if any.
    pub visual_selection: Option<VisualSelection>,
    /// Last known viewport height (updated each render frame).
    pub viewport_height: usize,
    /// Semantic color theme (borrowed from the application).
    pub theme: &'a Theme,
    /// Resolved icon set (borrowed from the application).
    pub icons: &'a IconSet,
}

// ── Construction & navigation ───────────────────────────────────────────

impl<'a> PanelState<'a> {
    /// Create a new panel for the given session.
    ///
    /// Starts with empty events, cursor at 0, tail attached, no horizontal
    /// scroll, not pinned.
    #[must_use]
    pub fn new(session_id: String, theme: &'a Theme, icons: &'a IconSet) -> Self {
        Self {
            session_id,
            events: Vec::new(),
            cursor: 0,
            scroll_offset: 0,
            tail_attached: true,
            horizontal_scroll: 0,
            pinned: false,
            language_servers: Vec::new(),
            expanded: HashSet::new(),
            visual_selection: None,
            viewport_height: 0,
            theme,
            icons,
        }
    }

    /// Total number of visible lines (flat lines including expanded detail).
    fn total_lines(&self) -> usize {
        self.flat_lines().len()
    }

    /// Load historical events. Sets cursor to the last event and attaches tail.
    pub fn load_events(&mut self, events: Vec<SessionEvent>) {
        self.events = events;
        self.expanded.clear();
        let total = self.total_lines();
        self.cursor = total.saturating_sub(1);
        self.tail_attached = true;
        self.snap_viewport(0);
    }

    /// Append a new event.
    ///
    /// If tail attached, advance cursor and scroll to keep the latest event
    /// visible. If detached, just append (cursor stays put).
    pub fn push_event(&mut self, event: SessionEvent) {
        self.events.push(event);
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

    /// Jump to first event — `g` key.
    pub const fn scroll_to_top(&mut self) {
        self.cursor = 0;
        self.scroll_offset = 0;
        self.tail_attached = false;
    }

    /// Jump to last event — `G` key.
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

    /// Compute the `(start, end)` indices of events visible in the viewport.
    ///
    /// `height` is the inner content height (excluding title bar and borders).
    #[must_use]
    pub fn visible_range(&self, height: usize) -> (usize, usize) {
        let total = self.total_lines();
        let start = self.scroll_offset.min(total);
        let end = (start + height).min(total);
        (start, end)
    }

    /// Scan events for `ServerState` events and update `language_servers`.
    ///
    /// Tracks the latest state per language.
    pub fn update_language_servers(&mut self) {
        let mut map: HashMap<String, LsState> = HashMap::new();
        let mut order: Vec<String> = Vec::new();

        for ev in &self.events {
            if let EventKind::ServerState { language, state } = &ev.kind {
                let ls_state = match state.as_str() {
                    "ready" | "running" => LsState::Healthy,
                    "initializing" | "starting" => LsState::Initializing,
                    "exited" | "crashed" | "error" | "stuck" => LsState::Crashed,
                    _ => LsState::NotLoaded,
                };
                if !map.contains_key(language) {
                    order.push(language.clone());
                }
                map.insert(language.clone(), ls_state);
            } else if let EventKind::Progress { language, .. } = &ev.kind {
                if !map.contains_key(language) {
                    order.push(language.clone());
                }
                map.insert(language.clone(), LsState::Progress);
            }
        }

        self.language_servers = order
            .into_iter()
            .filter_map(|name| {
                map.remove(&name)
                    .map(|state| LanguageServerStatus { name, state })
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

    /// Build a flat list of lines: event headers interleaved with detail lines
    /// for any expanded events.
    #[must_use]
    pub fn flat_lines(&self) -> Vec<FlatLine> {
        let collapsed = collapse_progress_indexed(&self.events);
        let mut lines = Vec::new();
        for &(event_index, ev) in &collapsed {
            lines.push(FlatLine::EventHeader { event_index });
            if self.expanded.contains(&event_index) {
                let count = detail_lines(ev, self.theme, self.icons).len();
                for detail_index in 0..count {
                    lines.push(FlatLine::Detail {
                        event_index,
                        detail_index,
                    });
                }
            }
        }
        lines
    }

    /// Quick check: does this event type have expandable detail?
    #[must_use]
    pub fn has_detail(&self, event_index: usize) -> bool {
        self.events
            .get(event_index)
            .is_some_and(|ev| match &ev.kind {
                EventKind::Diagnostics { count, .. } => *count > 0,
                EventKind::ToolResult { .. } => true,
                EventKind::ToolCall { params, file, .. } => params.is_some() || file.is_some(),
                _ => false,
            })
    }

    /// Toggle expansion of the event under the cursor.
    ///
    /// - On an `EventHeader`: toggle the event in/out of `expanded`.
    /// - On a `Detail` line: collapse the parent event, move cursor to its header.
    /// - On an event with no detail: no-op.
    pub fn toggle_expansion(&mut self) {
        let flat = self.flat_lines();
        let Some(current) = flat.get(self.cursor) else {
            return;
        };
        match *current {
            FlatLine::EventHeader { event_index } => {
                if !self.has_detail(event_index) {
                    return;
                }
                if self.expanded.contains(&event_index) {
                    self.expanded.remove(&event_index);
                } else {
                    self.expanded.insert(event_index);
                }
            }
            FlatLine::Detail { event_index, .. } => {
                self.expanded.remove(&event_index);
                // Move cursor to the parent header.
                let new_flat = self.flat_lines();
                if let Some(pos) = new_flat.iter().position(|fl| {
                    matches!(fl, FlatLine::EventHeader { event_index: ei } if *ei == event_index)
                }) {
                    self.cursor = pos;
                }
            }
        }
        self.snap_viewport(0);
    }
}

// ── Expansion helpers ───────────────────────────────────────────────────

/// Collapse consecutive progress events, preserving original indices into
/// the events vec.
fn collapse_progress_indexed(events: &[SessionEvent]) -> Vec<(usize, &SessionEvent)> {
    let mut result: Vec<(usize, &SessionEvent)> = Vec::with_capacity(events.len());
    for (idx, ev) in events.iter().enumerate() {
        if matches!(ev.kind, EventKind::McpMessage { .. }) {
            continue;
        }
        if let EventKind::Progress {
            language, title, ..
        } = &ev.kind
            && let Some((_, last)) = result.last()
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
        result.push((idx, ev));
    }
    result
}

/// Generate styled detail lines for an expanded event.
///
/// Returns an empty vec for events with no expandable detail.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "each match arm is simple; splitting would obscure the mapping"
)]
pub fn detail_lines(event: &SessionEvent, theme: &Theme, icons: &IconSet) -> Vec<Line<'static>> {
    // Indent to align past the timestamp column ("HH:MM:SS  " = 10 chars).
    let indent = "          ";
    match &event.kind {
        EventKind::Diagnostics {
            file,
            count,
            preview,
        } => {
            if *count == 0 {
                return Vec::new();
            }
            let mut lines = Vec::new();
            let header = format!("{{\"file\": \"{file}\", \"count\": {count}}}");
            lines.push(Line::from(vec![
                Span::raw(indent.to_string()),
                Span::styled(header, theme.muted),
            ]));
            lines.push(Line::from(vec![
                Span::raw(indent.to_string()),
                Span::styled("───────────────────────", theme.muted),
            ]));
            for line in preview.lines().filter(|s| !s.trim().is_empty()) {
                let trimmed = line.trim();
                if trimmed.starts_with("fix:") {
                    lines.push(Line::from(vec![
                        Span::raw(format!("{indent}    ")),
                        Span::styled(trimmed.to_string(), theme.info),
                    ]));
                } else {
                    let (icon, style) = diag_style(1, trimmed, icons, theme);
                    lines.push(Line::from(vec![
                        Span::raw(indent.to_string()),
                        Span::styled(icon.to_string(), style),
                        Span::styled(trimmed.to_string(), style),
                    ]));
                }
            }
            lines
        }
        EventKind::ToolResult {
            tool,
            success,
            duration_ms,
            output,
            params,
        } => {
            let (status, style) = if *success {
                ("ok", theme.success)
            } else {
                ("error", theme.error)
            };
            let mut lines = vec![Line::from(vec![
                Span::raw(indent.to_string()),
                Span::styled(format!("{tool}: {status} ({duration_ms}ms)"), style),
            ])];
            if let Some(p) = params {
                let json = serde_json::to_string(p).unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::raw(indent.to_string()),
                    Span::styled(format!("request: {json}"), theme.muted),
                ]));
            }
            if params.is_some() && output.is_some() {
                lines.push(Line::from(vec![
                    Span::raw(indent.to_string()),
                    Span::styled("───────────────────────", theme.muted),
                ]));
            }
            if let Some(text) = output {
                for line in text.lines() {
                    if line.trim().is_empty() {
                        lines.push(Line::default());
                    } else {
                        lines.push(Line::from(vec![
                            Span::raw(indent.to_string()),
                            Span::styled(line.to_string(), theme.muted),
                        ]));
                    }
                }
            }
            lines
        }
        EventKind::ToolCall { params, file, .. } => {
            let mut lines = Vec::new();
            if let Some(p) = params {
                let json = serde_json::to_string(p).unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::raw(indent.to_string()),
                    Span::styled(json, theme.muted),
                ]));
            }
            if let Some(path) = file {
                lines.push(Line::from(vec![
                    Span::raw(indent.to_string()),
                    Span::styled(path.clone(), theme.muted),
                ]));
            }
            lines
        }
        _ => Vec::new(),
    }
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

/// Status icon character for a language server state.
const fn ls_status_icon(state: &LsState) -> &'static str {
    match state {
        LsState::NotLoaded => "\u{25CB} ", // ○
        _ => "\u{25CF} ",                  // ●
    }
}

/// Build the title line for a panel.
fn build_title(state: &PanelState<'_>) -> Line<'static> {
    let id_short = if state.session_id.len() > 8 {
        &state.session_id[..8]
    } else {
        &state.session_id
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
            spans.push(Span::styled(ls_status_icon(&ls.state).to_string(), style));
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

/// Render a single events panel into the given buffer area.
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

    // Cache detail lines per expanded event to avoid recomputation.
    let mut detail_cache: HashMap<usize, Vec<Line<'static>>> = HashMap::new();

    // Render each visible line.
    let content_width = inner.width as usize;
    for (i, fl) in flat[start..end].iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }

        let line = match fl {
            FlatLine::EventHeader { event_index } => {
                format_event_styled(&state.events[*event_index], state.icons, state.theme)
            }
            FlatLine::Detail {
                event_index,
                detail_index,
            } => detail_cache
                .entry(*event_index)
                .or_insert_with(|| {
                    detail_lines(&state.events[*event_index], state.theme, state.icons)
                })
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
    use crate::session::EventKind;

    fn test_theme() -> Theme {
        Theme::new()
    }

    fn test_icons() -> IconSet {
        IconSet::from_config(IconConfig::default())
    }

    fn make_event(kind: EventKind) -> SessionEvent {
        SessionEvent {
            timestamp: chrono::Utc::now(),
            kind,
        }
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
        assert!(panel.events.is_empty());
    }

    #[test]
    fn test_panel_load_events() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let events: Vec<SessionEvent> = (0..10).map(|_| make_event(EventKind::Started)).collect();
        panel.load_events(events);
        assert_eq!(panel.events.len(), 10);
        assert_eq!(panel.cursor, 9);
        assert!(panel.tail_attached);
    }

    #[test]
    fn test_panel_push_event_attached() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let events: Vec<SessionEvent> = (0..5).map(|_| make_event(EventKind::Started)).collect();
        panel.load_events(events);
        assert_eq!(panel.cursor, 4);

        panel.push_event(make_event(EventKind::Shutdown));
        assert_eq!(panel.events.len(), 6);
        assert_eq!(panel.cursor, 5);
        assert!(panel.tail_attached);
    }

    #[test]
    fn test_panel_push_event_detached() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let events: Vec<SessionEvent> = (0..5).map(|_| make_event(EventKind::Started)).collect();
        panel.load_events(events);

        // Navigate up to detach.
        panel.navigate(-1);
        assert!(!panel.tail_attached);
        let cursor_before = panel.cursor;

        panel.push_event(make_event(EventKind::Shutdown));
        assert_eq!(panel.events.len(), 6);
        assert_eq!(panel.cursor, cursor_before);
        assert!(!panel.tail_attached);
    }

    #[test]
    fn test_panel_navigate_up_detaches() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let events: Vec<SessionEvent> = (0..10).map(|_| make_event(EventKind::Started)).collect();
        panel.load_events(events);
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
        let events: Vec<SessionEvent> = (0..5).map(|_| make_event(EventKind::Started)).collect();
        panel.load_events(events);

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
        let events: Vec<SessionEvent> = (0..20).map(|_| make_event(EventKind::Started)).collect();
        panel.load_events(events);

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
        let events: Vec<SessionEvent> = (0..20).map(|_| make_event(EventKind::Started)).collect();
        panel.load_events(events);

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
        let events: Vec<SessionEvent> = (0..100).map(|_| make_event(EventKind::Started)).collect();
        panel.load_events(events);

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
        let events: Vec<SessionEvent> = (0..100).map(|_| make_event(EventKind::Started)).collect();
        panel.load_events(events);

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
        let events: Vec<SessionEvent> = (0..100).map(|_| make_event(EventKind::Started)).collect();
        panel.load_events(events);

        // Cursor near bottom.
        panel.cursor = 97;
        panel.snap_viewport(20);

        let (start, end) = panel.visible_range(20);
        assert_eq!(end, 100);
        assert_eq!(start, 80);
    }

    #[test]
    fn test_panel_render_events() {
        let theme = test_theme();
        let icons = test_icons();
        let events: Vec<SessionEvent> = vec![
            make_event(EventKind::ToolCall {
                tool: "grep".to_string(),
                file: None,
                params: None,
            }),
            make_event(EventKind::ToolCall {
                tool: "glob".to_string(),
                file: Some("/src/lib.rs".to_string()),
                params: None,
            }),
        ];

        let mut panel = PanelState::new("test1234".to_string(), &theme, &icons);
        panel.load_events(events);

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
        let events: Vec<SessionEvent> = (0..5)
            .map(|_| {
                make_event(EventKind::ToolCall {
                    tool: "grep".to_string(),
                    file: None,
                    params: None,
                })
            })
            .collect();

        let mut panel = PanelState::new("test1234".to_string(), &theme, &icons);
        panel.load_events(events);
        // Set cursor to row 1 (second event in visible area).
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
        panel.events = vec![
            make_event(EventKind::ServerState {
                language: "rust".to_string(),
                state: "ready".to_string(),
            }),
            make_event(EventKind::ServerState {
                language: "ts".to_string(),
                state: "initializing".to_string(),
            }),
        ];

        panel.update_language_servers();
        assert_eq!(panel.language_servers.len(), 2);
        assert_eq!(panel.language_servers[0].name, "rust");
        assert_eq!(panel.language_servers[0].state, LsState::Healthy);
        assert_eq!(panel.language_servers[1].name, "ts");
        assert_eq!(panel.language_servers[1].state, LsState::Initializing);
    }

    // ── Expansion tests (ticket 04) ─────────────────────────────────────

    /// Build a diagnostic preview in the format produced by
    /// `format_diagnostics_compact` (one `  line:col [severity] source: msg`
    /// per diagnostic, joined by newlines).
    fn diag_preview(entries: &[(&str, u32, &str)]) -> String {
        entries
            .iter()
            .map(|(sev, line, msg)| format!("  {line}:1 [{sev}] rustc: {msg}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn test_flat_lines_no_expansion() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let events: Vec<SessionEvent> = (0..5).map(|_| make_event(EventKind::Started)).collect();
        panel.load_events(events);

        let flat = panel.flat_lines();
        assert_eq!(flat.len(), 5);
        for (i, fl) in flat.iter().enumerate() {
            assert_eq!(*fl, FlatLine::EventHeader { event_index: i });
        }
    }

    #[test]
    fn test_flat_lines_one_expanded() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let events = vec![
            make_event(EventKind::Started),
            make_event(EventKind::Diagnostics {
                file: "/src/lib.rs".to_string(),
                count: 3,
                preview: diag_preview(&[
                    ("error", 12, "one"),
                    ("warning", 34, "two"),
                    ("error", 56, "three"),
                ]),
            }),
            make_event(EventKind::Shutdown),
        ];
        panel.load_events(events);
        panel.expanded.insert(1);

        let flat = panel.flat_lines();
        // 3 headers + 5 detail lines (2 header/separator + 3 diag entries)
        assert_eq!(flat.len(), 8);
        assert_eq!(flat[0], FlatLine::EventHeader { event_index: 0 });
        assert_eq!(flat[1], FlatLine::EventHeader { event_index: 1 });
        assert_eq!(
            flat[2],
            FlatLine::Detail {
                event_index: 1,
                detail_index: 0
            }
        );
        assert_eq!(
            flat[3],
            FlatLine::Detail {
                event_index: 1,
                detail_index: 1
            }
        );
        assert_eq!(
            flat[4],
            FlatLine::Detail {
                event_index: 1,
                detail_index: 2
            }
        );
        assert_eq!(
            flat[5],
            FlatLine::Detail {
                event_index: 1,
                detail_index: 3
            }
        );
        assert_eq!(
            flat[6],
            FlatLine::Detail {
                event_index: 1,
                detail_index: 4
            }
        );
        assert_eq!(flat[7], FlatLine::EventHeader { event_index: 2 });
    }

    #[test]
    fn test_flat_lines_multiple_expanded() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let events = vec![
            make_event(EventKind::Diagnostics {
                file: "/a.rs".to_string(),
                count: 2,
                preview: diag_preview(&[("error", 1, "a"), ("warning", 2, "b")]),
            }),
            make_event(EventKind::Started),
            make_event(EventKind::Diagnostics {
                file: "/b.rs".to_string(),
                count: 1,
                preview: diag_preview(&[("error", 5, "c")]),
            }),
        ];
        panel.load_events(events);
        panel.expanded.insert(0);
        panel.expanded.insert(2);

        let flat = panel.flat_lines();
        // H0, D0.0..D0.3 (hdr+sep+2 diags), H1, H2, D2.0..D2.2 (hdr+sep+1 diag)
        assert_eq!(flat.len(), 10);
        assert_eq!(flat[0], FlatLine::EventHeader { event_index: 0 });
        for i in 0..4 {
            assert_eq!(
                flat[1 + i],
                FlatLine::Detail {
                    event_index: 0,
                    detail_index: i
                }
            );
        }
        assert_eq!(flat[5], FlatLine::EventHeader { event_index: 1 });
        assert_eq!(flat[6], FlatLine::EventHeader { event_index: 2 });
        for i in 0..3 {
            assert_eq!(
                flat[7 + i],
                FlatLine::Detail {
                    event_index: 2,
                    detail_index: i
                }
            );
        }
    }

    #[test]
    fn test_toggle_expansion_header() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let events = vec![
            make_event(EventKind::Started),
            make_event(EventKind::Diagnostics {
                file: "/a.rs".to_string(),
                count: 2,
                preview: diag_preview(&[("error", 1, "a"), ("warning", 2, "b")]),
            }),
        ];
        panel.load_events(events);
        // Cursor on event 1 (the Diagnostics header).
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
        let events = vec![
            make_event(EventKind::Started),
            make_event(EventKind::Diagnostics {
                file: "/a.rs".to_string(),
                count: 2,
                preview: diag_preview(&[("error", 1, "a"), ("warning", 2, "b")]),
            }),
            make_event(EventKind::Shutdown),
        ];
        panel.load_events(events);
        panel.expanded.insert(1);
        // flat: [H0, H1, D1.0, D1.1, D1.2, D1.3, H2] → cursor at 5 (D1.3)
        panel.cursor = 5;

        panel.toggle_expansion();
        assert!(!panel.expanded.contains(&1));
        // After collapse: [H0, H1, H2] → header of event 1 is at index 1.
        assert_eq!(panel.cursor, 1);
    }

    #[test]
    fn test_toggle_expansion_no_detail() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let events = vec![make_event(EventKind::Progress {
            language: "rust".to_string(),
            title: "Indexing".to_string(),
            message: None,
            percentage: None,
        })];
        panel.load_events(events);
        panel.cursor = 0;

        panel.toggle_expansion();
        assert!(panel.expanded.is_empty());
    }

    #[test]
    fn test_cursor_walks_detail_lines() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let events = vec![
            make_event(EventKind::Started),
            make_event(EventKind::Diagnostics {
                file: "/a.rs".to_string(),
                count: 3,
                preview: diag_preview(&[("error", 1, "a"), ("warning", 2, "b"), ("error", 3, "c")]),
            }),
            make_event(EventKind::Shutdown),
        ];
        panel.load_events(events);
        panel.expanded.insert(1);

        // flat: [H0, H1, D1.0..D1.4 (hdr+sep+3 diags), H2]
        panel.cursor = 0;
        panel.tail_attached = false;

        let expected = [
            FlatLine::EventHeader { event_index: 0 },
            FlatLine::EventHeader { event_index: 1 },
            FlatLine::Detail {
                event_index: 1,
                detail_index: 0,
            },
            FlatLine::Detail {
                event_index: 1,
                detail_index: 1,
            },
            FlatLine::Detail {
                event_index: 1,
                detail_index: 2,
            },
            FlatLine::Detail {
                event_index: 1,
                detail_index: 3,
            },
            FlatLine::Detail {
                event_index: 1,
                detail_index: 4,
            },
            FlatLine::EventHeader { event_index: 2 },
        ];

        let flat = panel.flat_lines();
        assert_eq!(flat[panel.cursor], expected[0]);
        for exp in &expected[1..] {
            panel.navigate(1);
            let flat = panel.flat_lines();
            assert_eq!(flat[panel.cursor], *exp);
        }
    }

    #[test]
    fn test_detail_lines_diagnostics() {
        let ev = make_event(EventKind::Diagnostics {
            file: "/src/lib.rs".to_string(),
            count: 2,
            preview: diag_preview(&[("error", 12, "something"), ("warning", 34, "other")]),
        });
        let theme = test_theme();
        let icons = test_icons();

        let lines = detail_lines(&ev, &theme, &icons);
        // JSON header + separator + 2 diagnostic lines
        assert_eq!(lines.len(), 4);
        let hdr: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            hdr.contains("/src/lib.rs"),
            "header should contain file path"
        );
        assert!(hdr.contains("\"count\": 2"), "header should contain count");
        let sep: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(sep.contains("───"), "second line should be separator");
        let text0: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text0.contains("rustc: something"),
            "first detail should contain diagnostic message"
        );
        let text1: String = lines[3].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text1.contains("rustc: other"),
            "second detail should contain diagnostic message"
        );
    }

    #[test]
    fn test_detail_lines_with_fix_lines() {
        let preview = [
            "  12:1 [error] rustc: unused import",
            "  fix: Remove unused import",
            "  34:1 [warning] rustc: something",
        ]
        .join("\n");
        let ev = make_event(EventKind::Diagnostics {
            file: "/src/lib.rs".to_string(),
            count: 2,
            preview,
        });
        let theme = test_theme();
        let icons = test_icons();

        let lines = detail_lines(&ev, &theme, &icons);
        // JSON header + separator + 3 preview lines
        assert_eq!(lines.len(), 5);

        // Lines 0-1: JSON header + separator
        assert!(lines[0].spans.iter().any(|s| s.style == theme.muted));
        let sep: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(sep.contains("───"));

        // Line 2: normal diagnostic — has severity icon, error style
        let text0: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text0.contains("[error]"));
        assert!(lines[2].spans.iter().any(|s| s.style == theme.error));

        // Line 3: fix line — info style, deeper indentation, no severity icon
        let text1: String = lines[3].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text1.contains("fix: Remove unused import"));
        assert!(lines[3].spans.iter().any(|s| s.style == theme.info));
        // Should have 14 chars of indentation (10 + 4)
        let leading = &lines[3].spans[0];
        assert_eq!(
            leading.content.len(),
            14,
            "fix line should have 14-char indent"
        );

        // Line 4: normal diagnostic — warning style
        let text2: String = lines[4].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text2.contains("[warning]"));
        assert!(lines[4].spans.iter().any(|s| s.style == theme.warning));
    }

    #[test]
    fn test_detail_lines_tool_result_with_output() {
        let ev = make_event(EventKind::ToolResult {
            tool: "grep".to_string(),
            success: true,
            duration_ms: 42,
            output: Some("line1\nline2".to_string()),
            params: None,
        });
        let theme = test_theme();
        let icons = test_icons();

        let lines = detail_lines(&ev, &theme, &icons);
        // Status line + two output lines
        assert_eq!(lines.len(), 3);

        let status: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(status.contains("grep: ok (42ms)"));
        assert!(lines[0].spans.iter().any(|s| s.style == theme.success));

        let out1: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(out1.contains("line1"));
        assert!(lines[1].spans.iter().any(|s| s.style == theme.muted));

        let out2: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(out2.contains("line2"));
        assert!(lines[2].spans.iter().any(|s| s.style == theme.muted));
    }

    #[test]
    fn test_detail_lines_tool_result_without_output() {
        let ev = make_event(EventKind::ToolResult {
            tool: "glob".to_string(),
            success: false,
            duration_ms: 100,
            output: None,
            params: None,
        });
        let theme = test_theme();
        let icons = test_icons();

        let lines = detail_lines(&ev, &theme, &icons);
        // Status line only, no output lines
        assert_eq!(lines.len(), 1);

        let status: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(status.contains("glob: error (100ms)"));
        assert!(lines[0].spans.iter().any(|s| s.style == theme.error));
    }

    #[test]
    fn test_detail_lines_empty_for_progress() {
        let ev = make_event(EventKind::Progress {
            language: "rust".to_string(),
            title: "Indexing".to_string(),
            message: None,
            percentage: None,
        });
        let theme = test_theme();
        let icons = test_icons();

        let lines = detail_lines(&ev, &theme, &icons);
        assert!(lines.is_empty());
    }

    #[test]
    fn test_visible_range_with_expansion() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let mut events: Vec<SessionEvent> =
            (0..10).map(|_| make_event(EventKind::Started)).collect();
        // Replace event 5 with a Diagnostics that has 4 detail lines.
        events[5] = make_event(EventKind::Diagnostics {
            file: "/a.rs".to_string(),
            count: 4,
            preview: diag_preview(&[
                ("error", 1, "a"),
                ("error", 2, "b"),
                ("warning", 3, "c"),
                ("error", 4, "d"),
            ]),
        });
        panel.load_events(events);
        panel.expanded.insert(5);

        // Total flat lines: 10 headers + 6 details (hdr+sep+4 diags) = 16.
        let flat = panel.flat_lines();
        assert_eq!(flat.len(), 16);

        // Set cursor to 0, snap viewport.
        panel.cursor = 0;
        panel.snap_viewport(10);
        let (start, end) = panel.visible_range(10);
        assert_eq!(start, 0);
        assert_eq!(end, 10);
    }

    #[test]
    fn test_render_expanded_event() {
        let theme = test_theme();
        let icons = test_icons();
        let events = vec![make_event(EventKind::Diagnostics {
            file: "/src/lib.rs".to_string(),
            count: 2,
            preview: diag_preview(&[
                ("error", 12, "something wrong"),
                ("warning", 34, "might be bad"),
            ]),
        })];

        let mut panel = PanelState::new("test1234".to_string(), &theme, &icons);
        panel.load_events(events);
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
        // Detail lines should contain the diagnostic messages.
        assert!(
            content.contains("something wrong"),
            "expected first diagnostic detail"
        );
        assert!(
            content.contains("might be bad"),
            "expected second diagnostic detail"
        );
    }

    #[test]
    fn test_detail_lines_tool_call_with_params() {
        let ev = make_event(EventKind::ToolCall {
            tool: "grep".to_string(),
            file: Some("/src/main.rs".to_string()),
            params: Some(serde_json::json!({"pattern": "main"})),
        });
        let theme = test_theme();
        let icons = test_icons();

        let lines = detail_lines(&ev, &theme, &icons);
        // JSON params line + file path line
        assert_eq!(lines.len(), 2);

        let json: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(json.contains("\"pattern\""), "should contain param key");
        assert!(json.contains("\"main\""), "should contain param value");
        assert!(lines[0].spans.iter().any(|s| s.style == theme.muted));

        let path: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(path.contains("/src/main.rs"), "should contain file path");
    }

    #[test]
    fn test_detail_lines_tool_result_with_params_and_output() {
        let ev = make_event(EventKind::ToolResult {
            tool: "grep".to_string(),
            success: true,
            duration_ms: 42,
            output: Some("## Symbol results\nsrc/main.rs L42-L50".to_string()),
            params: Some(serde_json::json!({"pattern": "main"})),
        });
        let theme = test_theme();
        let icons = test_icons();

        let lines = detail_lines(&ev, &theme, &icons);
        // Status + request header + separator + 2 output lines = 5
        assert_eq!(lines.len(), 5);

        let status: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(status.contains("grep: ok (42ms)"));

        let req: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(req.contains("request:"), "should have request prefix");
        assert!(req.contains("\"pattern\""), "should contain param key");

        let sep: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(sep.contains("───"), "should have separator");

        let out0: String = lines[3].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(out0.contains("Symbol results"));
        let out1: String = lines[4].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(out1.contains("src/main.rs"));
    }

    #[test]
    fn test_detail_lines_diagnostics_with_header() {
        let ev = make_event(EventKind::Diagnostics {
            file: "/src/lib.rs".to_string(),
            count: 1,
            preview: diag_preview(&[("error", 5, "unused variable")]),
        });
        let theme = test_theme();
        let icons = test_icons();

        let lines = detail_lines(&ev, &theme, &icons);
        // JSON header + separator + 1 diagnostic line = 3
        assert_eq!(lines.len(), 3);

        let hdr: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(hdr.contains("/src/lib.rs"), "header should contain file");
        assert!(hdr.contains("\"count\": 1"), "header should contain count");
        assert!(lines[0].spans.iter().any(|s| s.style == theme.muted));

        let sep: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(sep.contains("───"), "should have separator");

        let diag: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(diag.contains("unused variable"));
    }
}

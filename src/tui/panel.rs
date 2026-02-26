// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Events panel: renders a list of session events with cursor, scroll offset,
//! tail attach/detach behavior, and horizontal scroll indicators.
//!
//! This is the core building block — later tickets add expansion (04),
//! multi-panel grid (05), scrollbar (06), and selection (07) on top of this.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Widget};
use unicode_width::UnicodeWidthStr;

use super::theme::{IconSet, Theme, collapse_progress, format_event_styled};
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

/// State for a single Events panel.
pub struct PanelState {
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
}

// ── Construction & navigation ───────────────────────────────────────────

impl PanelState {
    /// Create a new panel for the given session.
    ///
    /// Starts with empty events, cursor at 0, tail attached, no horizontal
    /// scroll, not pinned.
    #[must_use]
    pub const fn new(session_id: String) -> Self {
        Self {
            session_id,
            events: Vec::new(),
            cursor: 0,
            scroll_offset: 0,
            tail_attached: true,
            horizontal_scroll: 0,
            pinned: false,
            language_servers: Vec::new(),
        }
    }

    /// Total number of visible lines after progress collapse.
    fn total_lines(&self) -> usize {
        let refs: Vec<&SessionEvent> = self.events.iter().collect();
        collapse_progress(refs).len()
    }

    /// Load historical events. Sets cursor to the last event and attaches tail.
    pub fn load_events(&mut self, events: Vec<SessionEvent>) {
        self.events = events;
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
        use std::collections::HashMap;

        let mut map: HashMap<String, LsState> = HashMap::new();
        let mut order: Vec<String> = Vec::new();

        for ev in &self.events {
            if let EventKind::ServerState { language, state } = &ev.kind {
                let ls_state = match state.as_str() {
                    "ready" | "running" => LsState::Healthy,
                    "initializing" | "starting" => LsState::Initializing,
                    "exited" | "crashed" | "error" => LsState::Crashed,
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
    /// `height` of 0 means use the last known height (stored in `scroll_offset`
    /// context). The caller can pass a concrete height for rendering.
    fn snap_viewport(&mut self, height: usize) {
        // Use a reasonable default if height is 0 (pre-render snapping).
        let h = if height > 0 { height } else { 20 };
        let total = self.total_lines();
        if total <= h {
            self.scroll_offset = 0;
            return;
        }
        let target = self.cursor.saturating_sub(h / 2);
        self.scroll_offset = target.min(total.saturating_sub(h));
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
fn build_title(state: &PanelState) -> Line<'static> {
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
pub fn render_panel(
    state: &PanelState,
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    icons: &IconSet,
    focused: bool,
) {
    if area.width < 4 || area.height < 2 {
        return;
    }

    let border_style = if focused {
        theme.border_focused
    } else {
        theme.border_unfocused
    };

    let title = build_title(state);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.width < 2 || inner.height < 1 {
        return;
    }

    // Build collapsed event lines.
    let event_refs: Vec<&SessionEvent> = state.events.iter().collect();
    let collapsed = collapse_progress(event_refs);
    let all_lines: Vec<Line<'_>> = collapsed
        .iter()
        .map(|ev| format_event_styled(ev, icons, theme))
        .collect();

    // Viewport slicing.
    let height = inner.height as usize;
    let total = all_lines.len();
    let start = state.scroll_offset.min(total);
    let end = (start + height).min(total);
    let visible = &all_lines[start..end];

    // Render each visible line.
    let content_width = inner.width as usize;
    for (i, line) in visible.iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }

        let display_line = if state.horizontal_scroll > 0
            || UnicodeWidthStr::width(
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .as_str(),
            ) > content_width
        {
            clip_line_horizontal(line, state.horizontal_scroll, content_width)
        } else {
            to_owned_line(line)
        };

        // Apply cursor highlight to the entire row.
        let line_index = start + i;
        if line_index == state.cursor {
            // Set selection style on the entire row first.
            for x in inner.x..inner.x + inner.width {
                buf[(x, y)].set_style(theme.selection);
            }
        }

        buf.set_line(inner.x, y, &display_line, inner.width);

        // Re-apply selection style on top of content for cursor row.
        if line_index == state.cursor {
            for x in inner.x..inner.x + inner.width {
                buf[(x, y)].set_style(theme.selection);
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
        let panel = PanelState::new("abc123".to_string());
        assert!(panel.tail_attached);
        assert_eq!(panel.cursor, 0);
        assert_eq!(panel.scroll_offset, 0);
        assert_eq!(panel.horizontal_scroll, 0);
        assert!(!panel.pinned);
        assert!(panel.events.is_empty());
    }

    #[test]
    fn test_panel_load_events() {
        let mut panel = PanelState::new("abc123".to_string());
        let events: Vec<SessionEvent> = (0..10).map(|_| make_event(EventKind::Started)).collect();
        panel.load_events(events);
        assert_eq!(panel.events.len(), 10);
        assert_eq!(panel.cursor, 9);
        assert!(panel.tail_attached);
    }

    #[test]
    fn test_panel_push_event_attached() {
        let mut panel = PanelState::new("abc123".to_string());
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
        let mut panel = PanelState::new("abc123".to_string());
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
        let mut panel = PanelState::new("abc123".to_string());
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
        let mut panel = PanelState::new("abc123".to_string());
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
        let mut panel = PanelState::new("abc123".to_string());
        let events: Vec<SessionEvent> = (0..20).map(|_| make_event(EventKind::Started)).collect();
        panel.load_events(events);

        panel.scroll_to_top();
        assert_eq!(panel.cursor, 0);
        assert_eq!(panel.scroll_offset, 0);
        assert!(!panel.tail_attached);
    }

    #[test]
    fn test_panel_scroll_to_bottom() {
        let mut panel = PanelState::new("abc123".to_string());
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
        let mut panel = PanelState::new("abc123".to_string());
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
        let mut panel = PanelState::new("abc123".to_string());
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
        let mut panel = PanelState::new("abc123".to_string());
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
        let events: Vec<SessionEvent> = vec![
            make_event(EventKind::ToolCall {
                tool: "hover".to_string(),
                file: Some("/src/main.rs".to_string()),
            }),
            make_event(EventKind::ToolCall {
                tool: "definition".to_string(),
                file: Some("/src/lib.rs".to_string()),
            }),
            make_event(EventKind::ToolCall {
                tool: "search".to_string(),
                file: None,
            }),
        ];

        let mut panel = PanelState::new("test1234".to_string());
        panel.load_events(events);

        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_panel(&panel, area, f.buffer_mut(), &theme, &icons, true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(content.contains("hover"), "expected hover tool name");
        assert!(
            content.contains("definition"),
            "expected definition tool name"
        );
        assert!(content.contains("search"), "expected search tool name");
    }

    #[test]
    fn test_panel_render_empty() {
        let panel = PanelState::new("empty123".to_string());
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_panel(&panel, area, f.buffer_mut(), &theme, &icons, true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        // Should contain the title, no panic.
        assert!(content.contains("Events"), "expected title in empty panel");
    }

    #[test]
    fn test_panel_render_cursor_highlight() {
        let events: Vec<SessionEvent> = (0..5)
            .map(|_| {
                make_event(EventKind::ToolCall {
                    tool: "hover".to_string(),
                    file: None,
                })
            })
            .collect();

        let mut panel = PanelState::new("test1234".to_string());
        panel.load_events(events);
        // Set cursor to row 1 (second event in visible area).
        panel.cursor = 1;
        panel.snap_viewport(8);

        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_panel(&panel, area, f.buffer_mut(), &theme, &icons, true);
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
        let mut panel = PanelState::new("abc123".to_string());
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
}

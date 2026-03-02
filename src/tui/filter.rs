// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Filter system with history navigation and autocomplete for event panels.
//!
//! Supports local filters (single panel via `f`) and global filters (all
//! panels via `F`). Provides live-as-you-type filtering, filter history
//! with `Up`/`Down` navigation, and `Tab`-cycled autocomplete from history.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::theme::{Theme, format_event_plain};
use crate::session::SessionEvent;

/// Maximum number of autocomplete suggestions shown.
const MAX_SUGGESTIONS: usize = 5;

// ── Types ────────────────────────────────────────────────────────────────

/// Scope of a filter: local to one panel or global across all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterScope {
    /// Applies to a single panel (by index).
    Local(usize),
    /// Applies to all Events panels simultaneously.
    Global,
}

/// State for filter input and active filtering.
pub struct FilterState {
    /// Current text being typed.
    pub input: String,
    /// Locked (active) filter string, set on Enter.
    pub locked: Option<String>,
    /// Filter scope.
    pub scope: FilterScope,
    /// History of locked filter strings (most recent last).
    pub history: Vec<String>,
    /// Current position in history navigation (`None` = not navigating).
    pub history_cursor: Option<usize>,
    /// Current autocomplete suggestion index (`None` = no suggestion).
    pub suggestion_index: Option<usize>,
    /// Text saved before history navigation (to restore on cancel).
    pub saved_input: Option<String>,
}

// ── FilterState ──────────────────────────────────────────────────────────

impl FilterState {
    /// Create a new filter state with the given scope.
    ///
    /// Starts with empty input, no locked filter, and empty history.
    #[must_use]
    pub const fn new(scope: FilterScope) -> Self {
        Self {
            input: String::new(),
            locked: None,
            scope,
            history: Vec::new(),
            history_cursor: None,
            suggestion_index: None,
            saved_input: None,
        }
    }

    /// Append a character to the input.
    ///
    /// Resets suggestion and history cursor (typing cancels navigation).
    pub fn push_char(&mut self, c: char) {
        self.input.push(c);
        self.suggestion_index = None;
        self.history_cursor = None;
        self.saved_input = None;
    }

    /// Remove the last character from the input (backspace).
    ///
    /// Resets suggestion state.
    pub fn pop_char(&mut self) {
        self.input.pop();
        self.suggestion_index = None;
    }

    /// Lock the current filter (Enter).
    ///
    /// If a suggestion is active, materializes it into input first.
    /// Sets `locked` to the input (or `None` if empty). Adds to history
    /// with deduplication.
    pub fn submit(&mut self) {
        // Materialize active suggestion.
        if let Some(idx) = self.suggestion_index {
            let matches = self.matching_entries_owned();
            if let Some(suggestion) = matches.get(idx) {
                self.input.clone_from(suggestion);
            }
        }

        if self.input.is_empty() {
            self.locked = None;
        } else {
            self.locked = Some(self.input.clone());
            // Deduplicate: remove existing identical entry before appending.
            self.history.retain(|h| h != &self.input);
            self.history.push(self.input.clone());
        }

        self.input.clear();
        self.suggestion_index = None;
        self.history_cursor = None;
        self.saved_input = None;
    }

    /// Cancel filter input without locking (Esc while typing).
    pub fn cancel(&mut self) {
        self.input.clear();
        self.suggestion_index = None;
        self.history_cursor = None;
        self.saved_input = None;
    }

    /// Clear the locked filter (Esc with locked filter, not in input mode).
    pub fn clear_locked(&mut self) {
        self.locked = None;
    }

    /// Navigate filter history (`Up` = delta -1, `Down` = delta +1).
    ///
    /// On first navigation, saves the current input. Moving past the newest
    /// entry restores the saved input. Clamps at boundaries (no wrap).
    #[allow(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        reason = "history indices never overflow isize"
    )]
    pub fn navigate_history(&mut self, delta: isize) {
        if self.history.is_empty() {
            return;
        }

        // Save input on first navigation.
        if self.history_cursor.is_none() {
            self.saved_input = Some(self.input.clone());
        }

        let len = self.history.len() as isize;

        let new_pos = self
            .history_cursor
            .map_or_else(|| len + delta, |pos| pos as isize + delta);

        if new_pos >= len {
            // Past newest — restore saved input.
            self.history_cursor = None;
            if let Some(saved) = self.saved_input.take() {
                self.input = saved;
            }
        } else if new_pos < 0 {
            // At oldest — clamp.
            self.history_cursor = Some(0);
            self.input = self.history[0].clone();
        } else {
            let idx = new_pos as usize;
            self.history_cursor = Some(idx);
            self.input = self.history[idx].clone();
        }

        self.suggestion_index = None;
    }

    /// Cycle autocomplete suggestions (`Tab` = delta -1, `Shift+Tab` = delta +1).
    ///
    /// Tab enters the suggestion list at the most recent match (index 0)
    /// and advances through older entries. `Shift+Tab` reverses.
    ///
    /// Both directions wrap: Tab past the oldest entry returns to null
    /// (raw input), Shift+Tab past the most recent also returns to null.
    /// The cycle is: null → 0 (most recent) → 1 → ... → null.
    pub fn cycle_suggestion(&mut self, delta: isize) {
        let matches = self.matching_entries_owned();
        if matches.is_empty() {
            self.suggestion_index = None;
            return;
        }

        let count = matches.len();
        // Tab (delta -1): null → 0 → 1 → ... → count-1 → null
        // Shift+Tab (delta +1): null → count-1 → ... → 1 → 0 → null
        match (self.suggestion_index, delta.signum()) {
            (None, -1) => self.suggestion_index = Some(0),
            (None, 1) => self.suggestion_index = Some(count - 1),
            (Some(idx), -1) if idx + 1 >= count => self.suggestion_index = None,
            (Some(idx), -1) => self.suggestion_index = Some(idx + 1),
            (Some(0), 1) => self.suggestion_index = None,
            (Some(idx), 1) => self.suggestion_index = Some(idx - 1),
            _ => {}
        }
    }

    /// Return history entries matching the current input.
    ///
    /// Entries that start with the input are preferred, followed by entries
    /// that contain the input. Deduplicated, most recent first, max 5.
    #[must_use]
    pub fn matching_entries(&self) -> Vec<&str> {
        if self.input.is_empty() {
            return self
                .history
                .iter()
                .rev()
                .map(String::as_str)
                .take(MAX_SUGGESTIONS)
                .collect();
        }

        let lower_input = self.input.to_lowercase();
        let mut seen = std::collections::HashSet::new();
        let mut result: Vec<&str> = Vec::new();

        // Most recent first — iterate in reverse.
        for entry in self.history.iter().rev() {
            let lower_entry = entry.to_lowercase();
            if lower_entry.contains(&lower_input) && seen.insert(entry.as_str()) {
                result.push(entry.as_str());
                if result.len() >= MAX_SUGGESTIONS {
                    break;
                }
            }
        }

        result
    }

    /// The text to use for filtering.
    ///
    /// If a suggestion is active, returns the full suggestion text.
    /// Otherwise returns the typed input.
    #[must_use]
    pub fn effective_input(&self) -> &str {
        if let Some(idx) = self.suggestion_index {
            let matches = self.matching_entries();
            if let Some(entry) = matches.get(idx) {
                return entry;
            }
        }
        &self.input
    }

    /// Ghost text suffix beyond what the user has typed.
    ///
    /// Returns `Some(suffix)` if a suggestion is active and extends
    /// beyond the typed input. `None` if no suggestion.
    #[must_use]
    pub fn virtual_text(&self) -> Option<&str> {
        let idx = self.suggestion_index?;
        let matches = self.matching_entries();
        let entry = matches.get(idx)?;
        if entry.len() > self.input.len() && entry.starts_with(&self.input) {
            Some(&entry[self.input.len()..])
        } else {
            // Suggestion doesn't extend input (contains-match, not prefix-match).
            None
        }
    }

    /// Internal helper: owned version of `matching_entries()` for use in
    /// methods that need to avoid borrowing `&self` conflicts.
    fn matching_entries_owned(&self) -> Vec<String> {
        self.matching_entries()
            .into_iter()
            .map(String::from)
            .collect()
    }
}

// ── Filtering ────────────────────────────────────────────────────────────

/// Return indices of events matching the pattern.
///
/// Performs a case-insensitive substring match on the plain-text
/// representation of each event (via [`format_event_plain`]).
#[must_use]
pub fn filter_events(events: &[SessionEvent], pattern: &str) -> Vec<usize> {
    let lower_pattern = pattern.to_lowercase();
    events
        .iter()
        .enumerate()
        .filter(|(_, ev)| {
            format_event_plain(ev)
                .to_lowercase()
                .contains(&lower_pattern)
        })
        .map(|(i, _)| i)
        .collect()
}

// ── Rendering ────────────────────────────────────────────────────────────

/// Render the filter input bar into a 1-row area.
///
/// Layout: `<prefix><input>▏<virtual_text>      Esc cancel`
///
/// - Prefix is "Filter: " for local scope, "Global: " for global.
/// - Virtual text is rendered in dim style when a suggestion is active.
/// - "Esc cancel" is right-aligned.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
pub fn render_filter_bar(state: &FilterState, area: Rect, buf: &mut Buffer, theme: &Theme) {
    if area.width < 10 || area.height < 1 {
        return;
    }

    let width = area.width as usize;
    let y = area.y;

    let prefix = match state.scope {
        FilterScope::Local(_) => "Filter: ",
        FilterScope::Global => "Global: ",
    };

    let hint = "Esc cancel";

    // Build the left side: prefix + input + cursor + virtual text.
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(prefix.to_string(), theme.accent));
    spans.push(Span::raw(state.input.clone()));
    spans.push(Span::styled("\u{258F}", theme.text)); // ▏ cursor

    if let Some(vtext) = state.virtual_text() {
        spans.push(Span::styled(vtext.to_string(), theme.muted));
    }

    let left_line = Line::from(spans);
    let left_width = UnicodeWidthStr::width(
        left_line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
            .as_str(),
    );

    // Render left content.
    buf.set_line(area.x, y, &left_line, area.width);

    // Render right-aligned hint if there's room.
    let hint_width = UnicodeWidthStr::width(hint);
    let gap = 1; // minimum gap between content and hint
    if left_width + gap + hint_width <= width {
        let hint_x = area.x + area.width - hint_width as u16;
        let hint_line = Line::from(Span::styled(hint.to_string(), theme.muted));
        buf.set_line(hint_x, y, &hint_line, hint_width as u16);
    }
}

/// Render the autocomplete liftup above the filter bar.
///
/// Up to 5 matching entries. Most recent closest to the filter input
/// (at the bottom of the liftup). Selected entry has "▐" prefix.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
pub fn render_filter_liftup(state: &FilterState, area: Rect, buf: &mut Buffer, theme: &Theme) {
    let matches = state.matching_entries();
    if matches.is_empty() || area.height < 1 {
        return;
    }

    let visible_count = matches.len().min(area.height as usize);

    // Entries are most-recent-first in the vec. We want most-recent at the
    // bottom of the liftup (closest to the filter bar), so we reverse
    // for rendering: bottom row = matches[0], row above = matches[1], etc.
    for (i, entry) in matches.iter().take(visible_count).enumerate() {
        // Row position: bottom-aligned within the area.
        let row = area.y + (visible_count - 1 - i) as u16;
        if row < area.y {
            break;
        }

        let is_selected = state.suggestion_index == Some(i);
        let prefix = if is_selected { "\u{2590} " } else { "  " }; // ▐
        let style = if is_selected { theme.text } else { theme.muted };

        let line = Line::from(vec![
            Span::styled(prefix.to_string(), style),
            Span::styled((*entry).to_string(), style),
        ]);
        buf.set_line(area.x, row, &line, area.width);
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

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
    use crate::session::{EventKind, SessionEvent};
    use crate::tui::theme::IconSet;

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

    // ── 1. Push and submit ──────────────────────────────────────────────

    #[test]
    fn test_filter_push_and_submit() {
        let mut f = FilterState::new(FilterScope::Local(0));
        f.push_char('h');
        f.push_char('o');
        f.push_char('v');
        f.push_char('e');
        f.push_char('r');
        f.submit();
        assert_eq!(f.locked, Some("hover".to_string()));
        assert!(f.history.contains(&"hover".to_string()));
    }

    // ── 2. Cancel ───────────────────────────────────────────────────────

    #[test]
    fn test_filter_cancel() {
        let mut f = FilterState::new(FilterScope::Local(0));
        f.push_char('t');
        f.push_char('e');
        f.push_char('s');
        f.push_char('t');
        f.cancel();
        assert_eq!(f.locked, None);
        assert!(f.input.is_empty());
        assert!(f.history.is_empty());
    }

    // ── 3. Clear locked ─────────────────────────────────────────────────

    #[test]
    fn test_filter_clear_locked() {
        let mut f = FilterState::new(FilterScope::Local(0));
        for c in "hover".chars() {
            f.push_char(c);
        }
        f.submit();
        assert_eq!(f.locked, Some("hover".to_string()));

        f.clear_locked();
        assert_eq!(f.locked, None);
        assert!(f.history.contains(&"hover".to_string()));
    }

    // ── 4. History navigation ───────────────────────────────────────────

    #[test]
    fn test_filter_history_navigation() {
        let mut f = FilterState::new(FilterScope::Local(0));

        // Submit three entries.
        for s in &["aaa", "bbb", "ccc"] {
            for c in s.chars() {
                f.push_char(c);
            }
            f.submit();
        }

        // Start a new filter, navigate up.
        f.push_char('x');
        f.navigate_history(-1); // → ccc
        assert_eq!(f.input, "ccc");

        f.navigate_history(-1); // → bbb
        assert_eq!(f.input, "bbb");

        f.navigate_history(1); // → ccc
        assert_eq!(f.input, "ccc");

        f.navigate_history(1); // → back to original "x"
        assert_eq!(f.input, "x");
    }

    // ── 5. History dedup ────────────────────────────────────────────────

    #[test]
    fn test_filter_history_dedup() {
        let mut f = FilterState::new(FilterScope::Local(0));
        for _ in 0..2 {
            for c in "hover".chars() {
                f.push_char(c);
            }
            f.submit();
        }
        assert_eq!(
            f.history.iter().filter(|h| *h == "hover").count(),
            1,
            "history should contain only one 'hover' entry"
        );
    }

    // ── 6. Suggestion cycle ─────────────────────────────────────────────

    #[test]
    fn test_filter_suggestion_cycle() {
        let mut f = FilterState::new(FilterScope::Local(0));
        // Build history: ["hover ok", "hover error", "search"].
        for s in &["hover ok", "hover error", "search"] {
            for c in s.chars() {
                f.push_char(c);
            }
            f.submit();
        }

        // Type "hov".
        for c in "hov".chars() {
            f.push_char(c);
        }

        // Matches (most recent first): ["hover error", "hover ok"].
        let matches = f.matching_entries();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0], "hover error");
        assert_eq!(matches[1], "hover ok");

        // Tab (delta -1): enters at most recent match (index 0).
        f.cycle_suggestion(-1);
        assert_eq!(f.effective_input(), "hover error");

        // Tab again: advances to older match (index 1).
        f.cycle_suggestion(-1);
        assert_eq!(f.effective_input(), "hover ok");

        // Tab again: wraps back to null (no suggestion).
        f.cycle_suggestion(-1);
        assert_eq!(f.suggestion_index, None);
        assert_eq!(f.effective_input(), "hov");
    }

    // ── 7. Virtual text ─────────────────────────────────────────────────

    #[test]
    fn test_filter_virtual_text() {
        let mut f = FilterState::new(FilterScope::Local(0));
        for c in "hover".chars() {
            f.push_char(c);
        }
        f.submit();

        // Type "ho".
        for c in "ho".chars() {
            f.push_char(c);
        }

        // Cycle to suggestion.
        f.cycle_suggestion(-1);
        assert_eq!(f.virtual_text(), Some("ver"));
    }

    // ── 8. Effective input ──────────────────────────────────────────────

    #[test]
    fn test_filter_effective_input() {
        let mut f = FilterState::new(FilterScope::Local(0));
        for c in "hover error".chars() {
            f.push_char(c);
        }
        f.submit();

        // No suggestion: returns typed input.
        for c in "hov".chars() {
            f.push_char(c);
        }
        assert_eq!(f.effective_input(), "hov");

        // With suggestion: returns full suggestion.
        f.cycle_suggestion(-1);
        assert_eq!(f.effective_input(), "hover error");
    }

    // ── 9. Matching entries max 5 ───────────────────────────────────────

    #[test]
    fn test_filter_matching_entries_max_5() {
        let mut f = FilterState::new(FilterScope::Local(0));
        for i in 0..10 {
            let s = format!("test {i}");
            for c in s.chars() {
                f.push_char(c);
            }
            f.submit();
        }

        for c in "test".chars() {
            f.push_char(c);
        }
        let matches = f.matching_entries();
        assert!(
            matches.len() <= 5,
            "matching_entries should return at most 5, got {}",
            matches.len()
        );
    }

    // ── 10. filter_events case insensitive ──────────────────────────────

    #[test]
    fn test_filter_events_case_insensitive() {
        let events = vec![
            make_event(EventKind::ToolCall {
                tool: "Hover".to_string(),
                file: None,
                params: None,
            }),
            make_event(EventKind::ToolCall {
                tool: "hover".to_string(),
                file: None,
                params: None,
            }),
        ];

        let result = filter_events(&events, "HOVER");
        // Both should match.
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], 0);
        assert_eq!(result[1], 1);
    }

    // ── 11. filter_events no match ──────────────────────────────────────

    #[test]
    fn test_filter_events_no_match() {
        let events = vec![
            make_event(EventKind::Started),
            make_event(EventKind::Shutdown),
        ];

        let result = filter_events(&events, "zzzzz");
        assert!(result.is_empty());
    }

    // ── 12. Render filter bar typing ────────────────────────────────────

    #[test]
    fn test_render_filter_bar_typing() {
        let theme = test_theme();
        let _icons = test_icons();
        let mut f = FilterState::new(FilterScope::Local(0));
        for c in "hov".chars() {
            f.push_char(c);
        }

        let backend = TestBackend::new(60, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_filter_bar(&f, area, frame.buffer_mut(), &theme);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            content.contains("Filter: hov"),
            "expected 'Filter: hov' in bar, got: {content}"
        );
        assert!(
            content.contains("Esc cancel"),
            "expected 'Esc cancel' hint, got: {content}"
        );
    }

    // ── 13. Render filter bar with suggestion ───────────────────────────

    #[test]
    fn test_render_filter_bar_with_suggestion() {
        let theme = test_theme();
        let _icons = test_icons();
        let mut f = FilterState::new(FilterScope::Local(0));
        for c in "hover error".chars() {
            f.push_char(c);
        }
        f.submit();

        // Type "hov" and cycle to suggestion.
        for c in "hov".chars() {
            f.push_char(c);
        }
        f.cycle_suggestion(-1);

        let backend = TestBackend::new(60, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_filter_bar(&f, area, frame.buffer_mut(), &theme);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        // Should show "hov" followed by "er error" (the virtual text).
        assert!(
            content.contains("hov"),
            "expected typed text 'hov', got: {content}"
        );
        assert!(
            content.contains("er error"),
            "expected virtual text 'er error', got: {content}"
        );
    }

    // ── 14. Render filter liftup ───────────────────────────────────────

    #[test]
    fn test_render_filter_liftup() {
        let theme = test_theme();
        let _icons = test_icons();
        let mut f = FilterState::new(FilterScope::Local(0));

        // Build history with 3 matching entries.
        for s in &["hover ok", "hover error", "hover timeout"] {
            for c in s.chars() {
                f.push_char(c);
            }
            f.submit();
        }

        for c in "hover".chars() {
            f.push_char(c);
        }

        let backend = TestBackend::new(40, 5);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_filter_liftup(&f, area, frame.buffer_mut(), &theme);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            content.contains("hover ok"),
            "expected 'hover ok' in liftup, got: {content}"
        );
        assert!(
            content.contains("hover error"),
            "expected 'hover error' in liftup, got: {content}"
        );
        assert!(
            content.contains("hover timeout"),
            "expected 'hover timeout' in liftup, got: {content}"
        );
    }
}

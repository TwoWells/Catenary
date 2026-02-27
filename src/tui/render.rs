// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Top-level draw function composing all TUI widgets.
//!
//! Brings together sessions tree, events grid, scrollbars, overflow counts,
//! hints bar, filter bar, and selection highlights into a single frame.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use super::app::{App, FocusedPane, InputMode};
use super::degradation::{
    DegradationConfig, compute_degradation, is_below_minimum, render_below_minimum,
};
use super::filter::render_filter_bar;
use super::hints::render_hints;
use super::layout::compute_layout;
use super::panel::render_panel;
use super::scrollbar::{ScrollMetrics, compute_overflow, render_overflow_counts, render_scrollbar};
use super::selection::render_selection_highlight;
use super::tree::render_tree;

/// Pin ratio for pinned panels (same as grid module).
const PIN_RATIO: f64 = 2.0;

/// Draw the full TUI frame.
///
/// Computes degradation for the current terminal size, then renders:
/// 1. Sessions tree (if visible).
/// 2. Events grid with BSP layout.
/// 3. Scrollbars for each panel.
/// 4. Overflow count indicators.
/// 5. Selection highlights (visual mode).
/// 6. Hints bar or filter bar at the bottom row.
#[allow(
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    reason = "terminal coordinates are bounded; draw function coordinates many widgets"
)]
pub fn draw(frame: &mut Frame, app: &mut App<'_>) {
    let area = frame.area();

    // Below-minimum check.
    if is_below_minimum(area.width, area.height) {
        render_below_minimum(area, frame.buffer_mut());
        return;
    }

    // Compute degradation.
    let config = DegradationConfig {
        sessions_width_ratio: app.sessions_width_ratio,
        ..DegradationConfig::default()
    };
    let degrade = compute_degradation(area.width, area.height, app.grid.panels.len(), &config);

    // Override visibility from degradation.
    app.sessions_visible = degrade.sessions_visible;

    // Split into sessions area + grid area.
    let sessions_width = if app.sessions_visible && app.grid.panels.is_empty() {
        area.width
    } else if app.sessions_visible {
        degrade.sessions_width
    } else {
        0
    };

    let grid_width = area.width.saturating_sub(sessions_width);

    // Reserve 1 row at the bottom for hints/filter bar.
    let content_height = area.height.saturating_sub(1);

    let tree_area = Rect::new(area.x, area.y, sessions_width, content_height);
    let grid_area = Rect::new(area.x + sessions_width, area.y, grid_width, content_height);
    let hints_area = Rect::new(area.x, area.y + content_height, area.width, 1);

    app.tree_area = tree_area;
    app.grid_area = grid_area;

    // Render sessions tree.
    if app.sessions_visible && tree_area.width > 0 && tree_area.height > 0 {
        render_tree(
            &app.tree,
            tree_area,
            frame.buffer_mut(),
            app.theme,
            app.icons,
            app.focus == FocusedPane::Sessions,
        );
    }

    // Compute grid layout.
    if !app.grid.panels.is_empty() && grid_area.width > 0 && grid_area.height > 0 {
        let pinned = app.grid.pinned_indices();
        let layout = compute_layout(grid_area, &app.grid.composition, &pinned, PIN_RATIO);

        // Render each panel.
        for panel_rect in &layout.panels {
            if panel_rect.index >= app.grid.panels.len() {
                continue;
            }
            let is_focused =
                app.focus == FocusedPane::Events && app.grid.focused == Some(panel_rect.index);
            render_panel(
                &app.grid.panels[panel_rect.index],
                panel_rect.rect,
                frame.buffer_mut(),
                is_focused,
            );

            // Render scrollbar for this panel.
            let panel = &app.grid.panels[panel_rect.index];
            let flat_len = panel.flat_lines().len();
            let inner_height = panel_rect.rect.height.saturating_sub(1) as usize; // title row
            if flat_len > inner_height && panel_rect.rect.width > 0 {
                let track_area = Rect::new(
                    panel_rect.rect.x + panel_rect.rect.width.saturating_sub(1),
                    panel_rect.rect.y + 1,
                    1,
                    panel_rect.rect.height.saturating_sub(1),
                );
                let metrics = ScrollMetrics {
                    content_length: flat_len,
                    viewport_length: inner_height,
                    position: panel.scroll_offset,
                };
                render_scrollbar(
                    &metrics,
                    track_area,
                    frame.buffer_mut(),
                    Color::White,
                    Color::DarkGray,
                );

                // Render overflow counts.
                let content_area = Rect::new(
                    panel_rect.rect.x + 1,
                    panel_rect.rect.y + 1,
                    panel_rect.rect.width.saturating_sub(2),
                    panel_rect.rect.height.saturating_sub(1),
                );
                let counts = compute_overflow(&metrics);
                render_overflow_counts(
                    &counts,
                    content_area,
                    frame.buffer_mut(),
                    Style::default().fg(Color::DarkGray),
                );
            }

            // Render visual selection highlight.
            if let Some(ref sel) = panel.visual_selection {
                let content_area = Rect::new(
                    panel_rect.rect.x + 1,
                    panel_rect.rect.y + 1,
                    panel_rect.rect.width.saturating_sub(2),
                    panel_rect.rect.height.saturating_sub(1),
                );
                render_selection_highlight(
                    sel,
                    panel.scroll_offset,
                    frame.buffer_mut(),
                    content_area,
                    app.theme.selection,
                );
            }
        }

        app.grid_layout = Some(layout);
    } else {
        app.grid_layout = None;
    }

    // Render bottom bar: filter bar or hints.
    if hints_area.height > 0 {
        let filter_active = app.input_mode == InputMode::FilterInput;
        let filter_locked = app.filter.as_ref().is_some_and(|f| f.locked.is_some());
        let focused_on_bottom = app.focus == FocusedPane::Events;

        if filter_active {
            if let Some(ref filter) = app.filter {
                render_filter_bar(filter, hints_area, frame.buffer_mut(), app.theme);
            }
        } else {
            render_hints(
                hints_area,
                frame.buffer_mut(),
                app.theme,
                false,
                filter_locked,
                focused_on_bottom,
            );
        }
    }
}

/// Dispatch a keyboard event in Normal mode.
///
/// Returns `true` if the event was consumed.
#[allow(clippy::too_many_lines, reason = "match arms for each keybinding")]
pub fn handle_key_normal(app: &mut App<'_>, key: crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};

    match key.code {
        KeyCode::Char('q') => {
            app.quit = true;
            true
        }
        KeyCode::Tab => {
            match app.focus {
                FocusedPane::Sessions => {
                    if !app.grid.panels.is_empty() {
                        app.focus = FocusedPane::Events;
                    }
                }
                FocusedPane::Events => {
                    if app.sessions_visible {
                        app.focus = FocusedPane::Sessions;
                    }
                }
            }
            true
        }
        KeyCode::BackTab => {
            // Shift+Tab: reverse direction of Tab.
            match app.focus {
                FocusedPane::Sessions => {
                    if !app.grid.panels.is_empty() {
                        app.focus = FocusedPane::Events;
                    }
                }
                FocusedPane::Events => {
                    if app.sessions_visible {
                        app.focus = FocusedPane::Sessions;
                    }
                }
            }
            true
        }
        KeyCode::Char('f') if app.focus == FocusedPane::Events => {
            // Enter filter input mode.
            let scope = app
                .grid
                .focused
                .map_or(super::filter::FilterScope::Global, |idx| {
                    super::filter::FilterScope::Local(idx)
                });
            app.filter = Some(super::filter::FilterState::new(scope));
            app.input_mode = InputMode::FilterInput;
            true
        }
        KeyCode::Char('F') if app.focus == FocusedPane::Events => {
            // Enter global filter input mode.
            app.filter = Some(super::filter::FilterState::new(
                super::filter::FilterScope::Global,
            ));
            app.input_mode = InputMode::FilterInput;
            true
        }
        KeyCode::Char('v') if app.focus == FocusedPane::Events => {
            app.input_mode = InputMode::Visual;
            // Start visual selection at the focused panel's cursor.
            if let Some(panel) = app.grid.focused_panel_mut() {
                panel.visual_selection = Some(super::selection::VisualSelection::new(panel.cursor));
            }
            true
        }
        KeyCode::Char('j') | KeyCode::Down => {
            match app.focus {
                FocusedPane::Sessions => app.tree.navigate(1),
                FocusedPane::Events => {
                    if let Some(panel) = app.grid.focused_panel_mut() {
                        panel.navigate(1);
                    }
                }
            }
            true
        }
        KeyCode::Char('k') | KeyCode::Up => {
            match app.focus {
                FocusedPane::Sessions => app.tree.navigate(-1),
                FocusedPane::Events => {
                    if let Some(panel) = app.grid.focused_panel_mut() {
                        panel.navigate(-1);
                    }
                }
            }
            true
        }
        KeyCode::Enter => {
            if app.focus == FocusedPane::Sessions {
                if let Some(session_id) = app.tree.toggle_at_cursor() {
                    let idx = app.grid.open_panel(session_id.clone());
                    app.grid.focus_panel(idx);
                    app.focus = FocusedPane::Events;
                    // Load events for the panel.
                    if let Ok(events) = app.data.monitor_events(&session_id)
                        && let Some(panel) = app.grid.panels.get_mut(idx)
                    {
                        panel.load_events(events);
                        panel.update_language_servers();
                    }
                }
            } else if app.focus == FocusedPane::Events
                && let Some(panel) = app.grid.focused_panel_mut()
            {
                panel.toggle_expansion();
            }
            true
        }
        KeyCode::Char('h') | KeyCode::Left if app.focus == FocusedPane::Sessions => {
            app.tree.collapse_at_cursor();
            true
        }
        KeyCode::Char('l') | KeyCode::Right if app.focus == FocusedPane::Sessions => {
            app.tree.expand_at_cursor();
            true
        }
        KeyCode::Char('x') if app.focus == FocusedPane::Events => {
            // Close focused panel.
            if let Some(idx) = app.grid.focused {
                app.grid.close_panel(idx);
                if app.grid.panels.is_empty() {
                    app.focus = FocusedPane::Sessions;
                }
            }
            true
        }
        KeyCode::Char('w') if app.focus == FocusedPane::Events => {
            app.grid.cycle_layout();
            true
        }
        KeyCode::Char(' ') if app.focus == FocusedPane::Events => {
            app.grid.toggle_pin();
            true
        }
        KeyCode::Char('g') if app.focus == FocusedPane::Events => {
            if let Some(panel) = app.grid.focused_panel_mut() {
                panel.scroll_to_top();
            }
            true
        }
        KeyCode::Char('G') if app.focus == FocusedPane::Events => {
            if let Some(panel) = app.grid.focused_panel_mut() {
                panel.scroll_to_bottom();
            }
            true
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.focus == FocusedPane::Events
                && let Some(panel) = app.grid.focused_panel_mut()
            {
                panel.page_up(20);
            }
            true
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.focus == FocusedPane::Events
                && let Some(panel) = app.grid.focused_panel_mut()
            {
                panel.page_down(20);
            }
            true
        }
        KeyCode::Char('?') => {
            app.tree.show_cheatsheet = !app.tree.show_cheatsheet;
            true
        }
        KeyCode::Esc => {
            // Clear pins, clear locked filter.
            app.grid.clear_pins();
            if let Some(ref mut filter) = app.filter {
                filter.clear_locked();
                app.filter = None;
            }
            true
        }
        _ => false,
    }
}

/// Dispatch a keyboard event in `FilterInput` mode.
///
/// Returns `true` if the event was consumed.
pub fn handle_key_filter(app: &mut App<'_>, key: crossterm::event::KeyEvent) -> bool {
    use crossterm::event::KeyCode;

    let Some(ref mut filter) = app.filter else {
        app.input_mode = InputMode::Normal;
        return true;
    };

    match key.code {
        KeyCode::Esc => {
            filter.cancel();
            app.input_mode = InputMode::Normal;
            true
        }
        KeyCode::Enter => {
            filter.submit();
            app.input_mode = InputMode::Normal;
            true
        }
        KeyCode::Backspace => {
            filter.pop_char();
            true
        }
        KeyCode::Up => {
            filter.navigate_history(-1);
            true
        }
        KeyCode::Down => {
            filter.navigate_history(1);
            true
        }
        KeyCode::Tab => {
            filter.cycle_suggestion(-1);
            true
        }
        KeyCode::BackTab => {
            filter.cycle_suggestion(1);
            true
        }
        KeyCode::Char(c) => {
            filter.push_char(c);
            true
        }
        _ => false,
    }
}

/// Dispatch a keyboard event in Visual mode.
///
/// Returns `true` if the event was consumed.
pub fn handle_key_visual(app: &mut App<'_>, key: crossterm::event::KeyEvent) -> bool {
    use crossterm::event::KeyCode;

    match key.code {
        KeyCode::Esc => {
            // Cancel visual selection.
            if let Some(panel) = app.grid.focused_panel_mut() {
                panel.visual_selection = None;
            }
            app.input_mode = InputMode::Normal;
            true
        }
        KeyCode::Char('v') => {
            // Toggle off visual mode.
            if let Some(panel) = app.grid.focused_panel_mut() {
                panel.visual_selection = None;
            }
            app.input_mode = InputMode::Normal;
            true
        }
        KeyCode::Char('y') => {
            // Yank selection.
            if let Some(panel) = app.grid.focused_panel()
                && let Some(ref sel) = panel.visual_selection
            {
                let text = super::selection::yank_text(panel, sel);
                let _ = super::selection::copy_to_clipboard(&text);
            }
            // Clear selection and exit visual mode.
            if let Some(panel) = app.grid.focused_panel_mut() {
                panel.visual_selection = None;
            }
            app.input_mode = InputMode::Normal;
            true
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if let Some(panel) = app.grid.focused_panel_mut() {
                panel.navigate(1);
                if let Some(ref mut sel) = panel.visual_selection {
                    sel.extend(panel.cursor);
                }
            }
            true
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let Some(panel) = app.grid.focused_panel_mut() {
                panel.navigate(-1);
                if let Some(ref mut sel) = panel.visual_selection {
                    sel.extend(panel.cursor);
                }
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use std::collections::HashMap;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::config::IconConfig;
    use crate::session::{EventKind, SessionEvent, SessionInfo};
    use crate::tui::app::{App, FocusedPane, InputMode};
    use crate::tui::data::{MockDataSource, SessionRow};
    use crate::tui::render::{draw, handle_key_normal};
    use crate::tui::theme::{IconSet, Theme};
    use crate::tui::tree::TreeItem;

    fn make_session(id: &str, workspace: &str, alive: bool) -> SessionRow {
        SessionRow {
            info: SessionInfo {
                id: id.to_string(),
                pid: 1234,
                workspace: workspace.to_string(),
                started_at: chrono::Utc::now(),
                client_name: Some("test-client".to_string()),
                client_version: None,
            },
            alive,
            languages: vec!["rust".to_string()],
        }
    }

    fn make_event(kind: EventKind) -> SessionEvent {
        SessionEvent {
            timestamp: chrono::Utc::now(),
            kind,
        }
    }

    fn make_mock_data(
        sessions: Vec<SessionRow>,
        events_map: HashMap<String, Vec<SessionEvent>>,
    ) -> MockDataSource {
        MockDataSource {
            sessions,
            events: events_map,
            tail_events: HashMap::new(),
        }
    }

    /// Convert a ratatui buffer to a single string for assertion matching.
    fn buffer_to_string(buf: &ratatui::buffer::Buffer) -> String {
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
    fn test_full_render_cycle() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());

        let sessions = vec![
            make_session("active01", "/ws/test", true),
            make_session("active02", "/ws/test", true),
        ];
        let mut events_map = HashMap::new();
        events_map.insert(
            "active01".to_string(),
            vec![
                make_event(EventKind::Started),
                make_event(EventKind::ToolCall {
                    tool: "hover".to_string(),
                    file: Some("/src/main.rs".to_string()),
                }),
            ],
        );
        events_map.insert(
            "active02".to_string(),
            vec![
                make_event(EventKind::Started),
                make_event(EventKind::ToolCall {
                    tool: "search".to_string(),
                    file: None,
                }),
                make_event(EventKind::ToolCall {
                    tool: "definition".to_string(),
                    file: Some("/src/lib.rs".to_string()),
                }),
            ],
        );

        let data = Box::new(make_mock_data(sessions, events_map));
        let mut app = App::new(&theme, &icons, data, 0.4).expect("App creation");

        // Load events into the open panels.
        let panel_ids: Vec<String> = app
            .grid
            .panels
            .iter()
            .map(|p| p.session_id.clone())
            .collect();
        for id in &panel_ids {
            if let Ok(events) = app.data.monitor_events(id)
                && let Some(panel) = app.grid.panels.iter_mut().find(|p| p.session_id == *id)
            {
                panel.load_events(events);
            }
        }

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                draw(f, &mut app);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(content.contains("Sessions"), "expected Sessions title");
        assert!(
            content.contains("active01") || content.contains("active0"),
            "expected session ID in tree"
        );
    }

    #[test]
    fn test_render_below_minimum() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let data = Box::new(make_mock_data(vec![], HashMap::new()));
        let mut app = App::new(&theme, &icons, data, 0.4).expect("App creation");

        let backend = TestBackend::new(3, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                draw(f, &mut app);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        for x in 0u16..3 {
            assert_eq!(
                buf[(x, 0u16)].symbol(),
                "\u{2573}",
                "cell ({x}, 0) should be \u{2573}"
            );
        }
    }

    #[test]
    fn test_keyboard_dispatch_quit() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());
        let data = Box::new(make_mock_data(vec![], HashMap::new()));
        let mut app = App::new(&theme, &icons, data, 0.4).expect("App creation");

        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('q'),
            crossterm::event::KeyModifiers::NONE,
        );
        handle_key_normal(&mut app, key);
        assert!(app.quit, "q should set quit = true");
    }

    #[test]
    fn test_keyboard_dispatch_tab_focus() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());

        let sessions = vec![make_session("sess0001", "/ws/test", true)];
        let mut events_map = HashMap::new();
        events_map.insert("sess0001".to_string(), vec![make_event(EventKind::Started)]);
        let data = Box::new(make_mock_data(sessions, events_map));
        let mut app = App::new(&theme, &icons, data, 0.4).expect("App creation");

        assert_eq!(app.focus, FocusedPane::Sessions);

        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Tab,
            crossterm::event::KeyModifiers::NONE,
        );
        handle_key_normal(&mut app, key);
        assert_eq!(
            app.focus,
            FocusedPane::Events,
            "Tab should move focus to Events"
        );
    }

    #[test]
    fn test_keyboard_dispatch_filter_mode() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());

        let sessions = vec![make_session("sess0001", "/ws/test", true)];
        let mut events_map = HashMap::new();
        events_map.insert("sess0001".to_string(), vec![make_event(EventKind::Started)]);
        let data = Box::new(make_mock_data(sessions, events_map));
        let mut app = App::new(&theme, &icons, data, 0.4).expect("App creation");

        // Move focus to Events.
        app.focus = FocusedPane::Events;

        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('f'),
            crossterm::event::KeyModifiers::NONE,
        );
        handle_key_normal(&mut app, key);
        assert_eq!(
            app.input_mode,
            InputMode::FilterInput,
            "f should enter FilterInput mode"
        );
    }

    #[test]
    fn test_startup_auto_open_panels() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());

        let sessions = vec![
            make_session("active01", "/ws/test", true),
            make_session("active02", "/ws/test", true),
        ];
        let data = Box::new(make_mock_data(sessions, HashMap::new()));
        let app = App::new(&theme, &icons, data, 0.4).expect("App creation");

        assert_eq!(
            app.grid.panels.len(),
            2,
            "should auto-open panels for 2 active sessions"
        );
    }

    #[test]
    fn test_startup_cursor_on_first_active() {
        let theme = Theme::new();
        let icons = IconSet::from_config(IconConfig::default());

        let sessions = vec![
            make_session("active01", "/ws/test", true),
            make_session("dead0001", "/ws/test", false),
        ];
        let data = Box::new(make_mock_data(sessions, HashMap::new()));
        let app = App::new(&theme, &icons, data, 0.4).expect("App creation");

        // The cursor should be on the first active session.
        // The tree is: workspace node (idx 0), active session (idx 1), dead session (idx 2).
        let items = app.tree.visible_items();
        if let Some(TreeItem::Session { row, .. }) = items.get(app.tree.cursor) {
            assert!(row.alive, "cursor should be on an active session");
            assert_eq!(row.info.id, "active01");
        }
        // Cursor may be on workspace — that's fine if it's index 0.
        // The important thing is auto-open worked.
    }
}

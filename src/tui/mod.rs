// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Interactive TUI for browsing sessions and tailing events.
//!
//! Two-pane layout with focus tracking:
//! - **Left pane**: sessions tree with workspace grouping, active/dead
//!   indicators, and navigation.
//! - **Right pane**: multi-panel events grid with BSP layout, scrollbars,
//!   filtering, visual selection, and mouse support.
//!
//! All colors use the terminal's ANSI palette so the TUI inherits whatever
//! theme the user has configured.

pub mod app;
pub mod data;
pub mod degradation;
pub mod filter;
pub mod grid;
pub mod hints;
pub mod layout;
pub mod mouse;
pub mod panel;
pub mod render;
pub mod scrollbar;
pub mod selection;
pub mod theme;
pub mod tree;

pub use app::App;
pub use data::{DataSource, MockDataSource};

use std::io;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, MouseEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::config::IconConfig;
use crate::session::EventKind;

use self::app::{FocusedPane, InputMode};
use self::data::SqliteDataSource;
use self::mouse::{DragState, MouseAction};
use self::render::{draw, handle_key_filter, handle_key_normal, handle_key_visual};
use self::theme::{IconSet, Theme};

/// Tick interval for the event loop.
const TICK_INTERVAL: Duration = Duration::from_millis(200);

/// How often to refresh the session list (in ticks).
const SESSION_REFRESH_TICKS: u64 = 25; // 25 * 200ms = 5s

/// Run the interactive TUI with the live data source.
///
/// # Errors
///
/// Returns an error if terminal setup fails or session data cannot be read.
pub fn run(icon_config: IconConfig) -> Result<()> {
    let data = Box::new(SqliteDataSource::new()?);
    run_with_data(icon_config, data)
}

/// Run the interactive TUI with a provided data source.
///
/// This is the testable entry point — tests can inject a [`MockDataSource`].
///
/// # Errors
///
/// Returns an error if terminal setup fails or session data cannot be read.
pub fn run_with_data(icon_config: IconConfig, data: Box<dyn DataSource>) -> Result<()> {
    // Theme and icons live on the stack.
    let theme = Theme::detect();
    let icons = IconSet::from_config(icon_config);

    // Load TUI config for sessions width.
    let tui_config = crate::config::Config::load(None)
        .map(|c| c.tui)
        .unwrap_or_default();

    let mut app = App::new(&theme, &icons, data, tui_config.sessions_width)?;

    // Load events for auto-opened panels and create tails.
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
            panel.update_language_servers();
        }
        if let Ok(tail) = app.data.create_tail(id) {
            app.tails.insert(id.clone(), tail);
        }
    }

    // Terminal setup.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut app);

    // Terminal teardown.
    disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

/// Main event loop.
fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App<'_>,
) -> Result<()> {
    let mut last_tick = Instant::now();
    let mut tick_count = 0u64;

    loop {
        // Draw.
        terminal.draw(|f| draw(f, app))?;

        if app.quit {
            return Ok(());
        }

        // Poll for events.
        let timeout = TICK_INTERVAL
            .checked_sub(last_tick.elapsed())
            .unwrap_or_default();

        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) => {
                    let consumed = match app.input_mode {
                        InputMode::Normal => handle_key_normal(app, key),
                        InputMode::FilterInput => handle_key_filter(app, key),
                        InputMode::Visual => handle_key_visual(app, key),
                    };
                    let _ = consumed;
                }
                Event::Mouse(mouse) => {
                    handle_mouse(app, mouse);
                }
                _ => {}
            }
        }

        // Tick.
        if last_tick.elapsed() >= TICK_INTERVAL {
            last_tick = Instant::now();
            tick_count += 1;

            // Poll tails for new events every tick.
            poll_tails(app);

            // Refresh sessions periodically.
            if tick_count.is_multiple_of(SESSION_REFRESH_TICKS) {
                refresh_sessions(app);
            }
        }
    }
}

/// Handle mouse events by resolving actions and dispatching.
#[allow(
    clippy::cast_possible_wrap,
    reason = "mouse scroll deltas are small integers"
)]
fn handle_mouse(app: &mut App<'_>, mouse: event::MouseEvent) {
    let layout = app.grid_layout.as_ref();

    match mouse.kind {
        MouseEventKind::Down(event::MouseButton::Left) => {
            let Some(layout) = layout else { return };
            let overflow_counts: Vec<scrollbar::OverflowCounts> = app
                .grid
                .panels
                .iter()
                .map(|p| {
                    let flat_len = p.flat_lines().len();
                    let metrics = scrollbar::ScrollMetrics {
                        content_length: flat_len,
                        viewport_length: 20, // approximate
                        position: p.scroll_offset,
                    };
                    scrollbar::compute_overflow(&metrics)
                })
                .collect();

            let border_x = app.tree_area.x + app.tree_area.width;
            let action = mouse::resolve_click(
                mouse.column,
                mouse.row,
                app.tree_area,
                layout,
                border_x,
                0,
                &overflow_counts,
            );

            dispatch_mouse_action(app, &action);
        }
        MouseEventKind::ScrollDown => {
            let Some(layout) = layout else { return };
            let action = mouse::resolve_scroll(mouse.column, mouse.row, 3, app.tree_area, layout);
            dispatch_mouse_action(app, &action);
        }
        MouseEventKind::ScrollUp => {
            let Some(layout) = layout else { return };
            let action = mouse::resolve_scroll(mouse.column, mouse.row, -3, app.tree_area, layout);
            dispatch_mouse_action(app, &action);
        }
        MouseEventKind::Drag(event::MouseButton::Left) => {
            let Some(layout) = layout else { return };
            let action = mouse::resolve_drag(mouse.column, mouse.row, &app.drag_state, layout);
            dispatch_mouse_action(app, &action);
        }
        MouseEventKind::Up(event::MouseButton::Left) => {
            let action = mouse::resolve_release(&app.drag_state);
            dispatch_mouse_action(app, &action);
            app.drag_state = DragState::Idle;
        }
        _ => {}
    }
}

/// Dispatch a resolved mouse action.
#[allow(
    clippy::too_many_lines,
    reason = "match arms for each mouse action variant"
)]
fn dispatch_mouse_action(app: &mut App<'_>, action: &MouseAction) {
    match *action {
        MouseAction::FocusPanel(idx) => {
            app.focus = FocusedPane::Events;
            app.grid.focus_panel(idx);
        }
        MouseAction::FocusTree => {
            app.focus = FocusedPane::Sessions;
        }
        MouseAction::SelectSession { item } => {
            app.focus = FocusedPane::Sessions;
            app.tree.cursor = item;
        }
        MouseAction::ToggleExpansion { panel, line } => {
            app.focus = FocusedPane::Events;
            app.grid.focus_panel(panel);
            if let Some(p) = app.grid.panels.get_mut(panel) {
                p.cursor = line;
                p.toggle_expansion();
            }
        }
        MouseAction::TogglePin(idx) => {
            app.focus = FocusedPane::Events;
            app.grid.focus_panel(idx);
            app.grid.toggle_pin();
        }
        MouseAction::ScrollPanel { panel, delta } => {
            // Mouse scroll moves the viewport only, not the cursor or focus.
            if let Some(p) = app.grid.panels.get_mut(panel) {
                p.scroll_viewport(delta);
            }
        }
        MouseAction::ScrollTree(delta) => {
            app.tree.navigate(delta);
        }
        MouseAction::StartBorderDrag { x } => {
            app.drag_state = DragState::BorderResize { initial_x: x };
        }
        MouseAction::ContinueBorderDrag { x } => {
            let terminal_width = app.tree_area.width + app.grid_area.width;
            if let Some(new_width) = mouse::compute_sessions_width_from_drag(x, terminal_width, 20)
            {
                app.sessions_width_ratio = f64::from(new_width) / f64::from(terminal_width);
            } else {
                app.sessions_visible = false;
            }
        }
        MouseAction::EndBorderDrag => {
            app.drag_state = DragState::Idle;
        }
        MouseAction::StartDragSelect { panel, line } => {
            app.focus = FocusedPane::Events;
            app.grid.focus_panel(panel);
            if let Some(p) = app.grid.panels.get_mut(panel) {
                p.visual_selection = Some(selection::VisualSelection::new(line));
            }
            app.drag_state = DragState::LineSelect {
                panel,
                anchor: line,
            };
            app.input_mode = InputMode::Visual;
        }
        MouseAction::ContinueDragSelect { panel, line } => {
            if let Some(p) = app.grid.panels.get_mut(panel)
                && let Some(ref mut sel) = p.visual_selection
            {
                sel.extend(line);
            }
        }
        MouseAction::StartScrollbarDrag { panel, y } => {
            app.drag_state = DragState::Scrollbar { panel };
            scrollbar_click(app, panel, y);
        }
        MouseAction::ContinueScrollbarDrag { panel, y } => {
            scrollbar_click(app, panel, y);
        }
        MouseAction::JumpOverflow { panel, top } => {
            if let Some(p) = app.grid.panels.get_mut(panel) {
                if top {
                    p.scroll_offset = 0;
                } else {
                    let total = p.flat_lines().len();
                    p.scroll_offset = total.saturating_sub(20);
                }
                p.tail_attached = false;
            }
        }
        MouseAction::None => {}
    }
}

/// Handle a scrollbar click/drag at `y` for the given panel.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
fn scrollbar_click(app: &mut App<'_>, panel: usize, y: u16) {
    if let Some(layout) = &app.grid_layout
        && let Some(pr) = layout.panels.get(panel)
    {
        let track = ratatui::layout::Rect::new(
            pr.rect.x + pr.rect.width.saturating_sub(1),
            pr.rect.y + 1,
            1,
            pr.rect.height.saturating_sub(1),
        );
        if let Some(p) = app.grid.panels.get_mut(panel) {
            let metrics = scrollbar::ScrollMetrics {
                content_length: p.flat_lines().len(),
                viewport_length: track.height as usize,
                position: p.scroll_offset,
            };
            let pos = scrollbar::scroll_position_from_click(y, track, &metrics);
            p.scroll_offset = pos;
            p.tail_attached = false;
        }
    }
}

/// Poll all event tails and push new events into their panels.
fn poll_tails(app: &mut App<'_>) {
    let ids: Vec<String> = app.tails.keys().cloned().collect();
    for id in ids {
        let Some(tail) = app.tails.get_mut(&id) else {
            continue;
        };
        while let Ok(Some(event)) = tail.try_next_event() {
            let is_server_state = matches!(event.kind, EventKind::ServerState { .. });
            if let Some(panel) = app.grid.panels.iter_mut().find(|p| p.session_id == id) {
                panel.push_event(event);
                if is_server_state {
                    panel.update_language_servers();
                }
            }
        }
    }
}

/// Refresh the session list from the data source.
fn refresh_sessions(app: &mut App<'_>) {
    if let Ok(rows) = app.data.list_sessions() {
        let active_ids: Vec<String> = rows
            .iter()
            .filter(|r| r.alive)
            .map(|r| r.info.id.clone())
            .collect();

        // Preserve cursor position by session ID if possible.
        let cursor_session_id = app.tree.selected_session_id().map(String::from);

        app.tree = tree::SessionTree::from_sessions(rows);

        // Restore cursor position.
        if let Some(ref id) = cursor_session_id {
            for (i, item) in app.tree.visible_items().iter().enumerate() {
                if let tree::TreeItem::Session { row, .. } = item
                    && row.info.id == *id
                {
                    app.tree.cursor = i;
                    break;
                }
            }
        }

        // Update panel display IDs from refreshed session data.
        for panel in &mut app.grid.panels {
            for ws in &app.tree.workspaces {
                if let Some(row) = ws.sessions.iter().find(|s| s.info.id == panel.session_id) {
                    if let Some(ref csid) = row.info.client_session_id {
                        panel.display_id.clone_from(csid);
                    }
                    break;
                }
            }
        }

        // Auto-close panels for dead sessions.
        let dead_panel_indices: Vec<usize> = app
            .grid
            .panels
            .iter()
            .enumerate()
            .filter(|(_, p)| !active_ids.contains(&p.session_id))
            .map(|(i, _)| i)
            .collect();
        for idx in dead_panel_indices.into_iter().rev() {
            let session_id = app.grid.panels.get(idx).map(|p| p.session_id.clone());
            app.grid.close_panel(idx);
            if let Some(id) = session_id {
                app.tails.remove(&id);
            }
        }
        if app.grid.panels.is_empty() && app.focus == FocusedPane::Events {
            app.focus = FocusedPane::Sessions;
        }

        // Auto-open panels for new active sessions.
        for id in &active_ids {
            if app.grid.panel_for_session(id).is_none() {
                let idx = app.grid.open_panel(id.clone());
                if let Ok(events) = app.data.monitor_events(id)
                    && let Some(panel) = app.grid.panels.get_mut(idx)
                {
                    panel.load_events(events);
                    panel.update_language_servers();
                }
                if let Ok(tail) = app.data.create_tail(id) {
                    app.tails.insert(id.clone(), tail);
                }
            }
        }

        // Remove tails for panels that are no longer open.
        app.tails
            .retain(|id, _| app.grid.panel_for_session(id).is_some());
    }
}

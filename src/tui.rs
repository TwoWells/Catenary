// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Interactive TUI for browsing sessions and tailing events.
//!
//! Two-pane layout with focus tracking:
//! - **Top pane**: scrollable session list with active/dead indicators and
//!   language servers.
//! - **Bottom pane**: live, colored event tail for the selected session
//!   with a scrollbar.
//!
//! All colors use the terminal's ANSI palette so the TUI inherits whatever
//! theme the user has configured.

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Styled};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};
use std::fs;
use std::io;
use std::time::Duration;

use crate::config::{IconConfig, IconPreset};
use crate::session::{self, EventKind, SessionEvent, SessionInfo, TailReader};

// ── Theme ────────────────────────────────────────────────────────────────

/// Semantic color theme that defers to the terminal's ANSI palette.
///
/// Uses only base ANSI colors (`Color::Green`, `Color::Red`, etc.) and
/// modifiers (`DIM`, `BOLD`, `REVERSED`) so the TUI automatically inherits
/// whatever theme the user has configured in their terminal emulator.
struct Theme {
    // Chrome
    border_focused: Style,
    border_unfocused: Style,
    title: Style,
    hint_key: Style,
    hint_label: Style,
    selection: Style,

    // Sessions
    session_active: Style,
    session_dead: Style,
    session_meta: Style,

    // Events — semantic roles
    timestamp: Style,
    text: Style,
    accent: Style,
    success: Style,
    error: Style,
    warning: Style,
    info: Style,
    muted: Style,
    lock: Style,
    unlock: Style,
}

impl Theme {
    /// Build the default theme from the terminal's palette.
    const fn new() -> Self {
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
struct IconSet {
    diag_error: String,
    diag_warn: String,
    diag_info: String,
    diag_ok: String,
    lock: String,
    unlock: String,
    tool_search: String,
    tool_map: String,
    tool_hover: String,
    tool_goto: String,
    tool_refs: String,
    tool_diagnostics: String,
    tool_default: String,
}

impl IconSet {
    /// Resolve an [`IconConfig`] into a fully populated [`IconSet`].
    fn from_config(config: IconConfig) -> Self {
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

// ── Data model ───────────────────────────────────────────────────────────

/// Collected session row: info, liveness, and active language servers.
struct SessionRow {
    info: SessionInfo,
    alive: bool,
    languages: Vec<String>,
}

/// Which pane has focus.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FocusedPane {
    Sessions,
    Events,
}

/// What the user is currently doing.
enum InputMode {
    /// Normal navigation.
    Normal,
    /// Typing a filter string (the buffer is in `filter_input`).
    FilterInput,
}

/// Application state driving the TUI.
struct App {
    /// Semantic color theme.
    theme: Theme,
    /// Resolved icon theme.
    icons: IconSet,
    /// Which pane currently has focus.
    focus: FocusedPane,
    /// Layout areas from the last frame (for mouse hit-testing).
    sessions_area: Rect,
    /// Layout areas from the last frame (for mouse hit-testing).
    events_area: Rect,
    /// All known sessions (refreshed periodically).
    sessions: Vec<SessionRow>,
    /// Selection state for the session list.
    list_state: ListState,
    /// Events loaded for the currently selected session.
    events: Vec<SessionEvent>,
    /// Tail reader following the selected session's event file.
    tail: Option<TailReader>,
    /// ID of the session whose events are loaded.
    tailing_id: Option<String>,
    /// Active event filter (case-insensitive substring match on plain text).
    filter: Option<String>,
    /// Buffer while the user is typing a filter.
    filter_input: String,
    /// Current input mode.
    mode: InputMode,
    /// Transient status message shown in the events pane title.
    status: Option<String>,
    /// Height of the events pane (updated each frame).
    events_height: usize,
    /// Scroll offset from the bottom of the events list.
    /// `0` means "following" (pinned to the latest event).
    events_scroll: usize,
    /// Whether the user wants to quit.
    quit: bool,
}

impl App {
    fn new(icon_config: IconConfig) -> Result<Self> {
        let sessions = load_sessions()?;
        let mut list_state = ListState::default();
        if !sessions.is_empty() {
            list_state.select(Some(0));
        }
        Ok(Self {
            theme: Theme::new(),
            icons: IconSet::from_config(icon_config),
            focus: FocusedPane::Sessions,
            sessions_area: Rect::default(),
            events_area: Rect::default(),
            sessions,
            list_state,
            events: Vec::new(),
            tail: None,
            tailing_id: None,
            filter: None,
            filter_input: String::new(),
            mode: InputMode::Normal,
            status: None,
            events_height: 50,
            events_scroll: 0,
            quit: false,
        })
    }

    /// Refresh the session list from disk.
    fn refresh_sessions(&mut self) -> Result<()> {
        let selected_id = self.selected_session().map(|s| s.info.id.clone());
        self.sessions = load_sessions()?;

        // Preserve selection by ID if still present.
        if let Some(ref id) = selected_id {
            let pos = self.sessions.iter().position(|r| r.info.id == *id);
            self.list_state.select(pos.or(Some(0)));
        } else if !self.sessions.is_empty() {
            self.list_state.select(Some(0));
        }
        Ok(())
    }

    fn selected_session(&self) -> Option<&SessionRow> {
        self.list_state
            .selected()
            .and_then(|i| self.sessions.get(i))
    }

    fn move_selection(&mut self, delta: isize) {
        if self.sessions.is_empty() {
            return;
        }
        let len = self.sessions.len();
        let current = self.list_state.selected().unwrap_or(0);
        #[allow(
            clippy::cast_sign_loss,
            reason = "addition with len guarantees non-negative before modulo"
        )]
        #[allow(
            clippy::cast_possible_wrap,
            reason = "session list length will never approach isize::MAX"
        )]
        let next = (current.cast_signed() + delta).rem_euclid(len.cast_signed()) as usize;
        self.list_state.select(Some(next));
    }

    /// Ensure the tail reader tracks the currently selected session.
    fn sync_tail(&mut self) {
        let selected_id = self.selected_session().map(|s| s.info.id.clone());
        if selected_id == self.tailing_id {
            return;
        }
        // Selection changed — reset.
        self.events.clear();
        self.tail = None;
        self.tailing_id.clone_from(&selected_id);
        self.events_scroll = 0;

        if let Some(ref id) = selected_id {
            // Load all historical events so the user can scroll back freely.
            if let Ok(iter) = session::monitor_events(id) {
                self.events.extend(iter);
            }
            if let Ok(reader) = session::tail_events_new(id) {
                self.tail = Some(reader);
            }
        }
    }

    /// Drain any new events from the tail reader.
    fn poll_events(&mut self) {
        if let Some(ref mut reader) = self.tail {
            for _ in 0..50 {
                match reader.try_next_event() {
                    Ok(Some(ev)) => self.events.push(ev),
                    Ok(None) => break,
                    Err(_) => {
                        self.tail = None;
                        break;
                    }
                }
            }
        }
    }

    /// Return events filtered by the active filter (if any), with
    /// consecutive progress events for the same `(language, title)`
    /// collapsed down to the last one in each run.
    fn visible_events(&self) -> Vec<&SessionEvent> {
        let filtered: Vec<&SessionEvent> = self.filter.as_ref().map_or_else(
            || {
                self.events
                    .iter()
                    .filter(|ev| !matches!(ev.kind, EventKind::McpMessage { .. }))
                    .collect()
            },
            |pat| {
                let lower = pat.to_lowercase();
                self.events
                    .iter()
                    .filter(|ev| {
                        !matches!(ev.kind, EventKind::McpMessage { .. })
                            && format_event_plain(ev).to_lowercase().contains(&lower)
                    })
                    .collect()
            },
        );
        collapse_progress(filtered)
    }

    /// Scroll the events pane up (towards older events).
    const fn scroll_events_up(&mut self, amount: usize) {
        self.events_scroll = self.events_scroll.saturating_add(amount);
    }

    /// Scroll the events pane down (towards newer events), clamped to 0.
    const fn scroll_events_down(&mut self, amount: usize) {
        self.events_scroll = self.events_scroll.saturating_sub(amount);
    }

    /// Delete the currently selected session if it is dead.
    fn delete_selected(&mut self) {
        let Some(row) = self.selected_session() else {
            return;
        };
        if row.alive {
            self.status = Some("cannot delete an active session".to_string());
            return;
        }
        let id = row.info.id.clone();
        let dir = session::sessions_dir().join(&id);
        if let Err(e) = fs::remove_dir_all(&dir) {
            self.status = Some(format!("delete failed: {e}"));
            return;
        }
        self.status = Some(format!("deleted session {id}"));

        if self.tailing_id.as_deref() == Some(id.as_str()) {
            self.events.clear();
            self.tail = None;
            self.tailing_id = None;
        }
        let _ = self.refresh_sessions();
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Load sessions into display rows, including active language servers.
///
/// Active sessions are sorted to the top, with most recently started first
/// within each group.
fn load_sessions() -> Result<Vec<SessionRow>> {
    let raw = session::list_sessions()?;
    let mut rows: Vec<SessionRow> = raw
        .into_iter()
        .map(|(info, alive)| {
            let languages = session::active_languages(&info.id).unwrap_or_default();
            SessionRow {
                info,
                alive,
                languages,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.alive
            .cmp(&a.alive)
            .then_with(|| b.info.started_at.cmp(&a.info.started_at))
    });
    Ok(rows)
}

/// Collapse consecutive progress events with the same `(language, title)`
/// into just the last event of each run.
fn collapse_progress(events: Vec<&SessionEvent>) -> Vec<&SessionEvent> {
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
fn format_ago(started: chrono::DateTime<chrono::Utc>) -> String {
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
fn format_event_plain(ev: &SessionEvent) -> String {
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
fn tool_icon<'a>(name: &str, icons: &'a IconSet) -> &'a str {
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
fn basename(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

/// Determine the diagnostic icon and style from the count and preview text.
fn diag_style<'a>(
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

/// Build a styled `Line` for a single event.
#[allow(clippy::too_many_lines, reason = "match arms for each event kind")]
fn format_event_styled(ev: &SessionEvent, icons: &IconSet, theme: &Theme) -> Line<'static> {
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

/// Build the events pane title string.
fn build_events_title(app: &App) -> String {
    if let Some(ref msg) = app.status {
        return format!(" {msg} ");
    }
    app.tailing_id
        .as_ref()
        .map_or_else(|| " Events ".to_string(), |id| format!(" Events — {id} "))
}

/// Test whether a point is inside a rect.
const fn is_inside(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

// ── TUI entry point ─────────────────────────────────────────────────────

/// Run the interactive TUI.
///
/// # Errors
///
/// Returns an error if terminal setup fails or session data cannot be read.
pub fn run(icon_config: IconConfig) -> Result<()> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(icon_config)?;
    app.sync_tail();

    let result = run_loop(&mut terminal, &mut app);

    // Restore terminal regardless of outcome.
    terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

/// Main event loop.
fn run_loop(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let tick_rate = Duration::from_millis(200);

    loop {
        terminal.draw(|f| draw(f, app))?;

        if event::poll(tick_rate)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match app.mode {
                    InputMode::Normal => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
                        KeyCode::Tab => {
                            app.focus = match app.focus {
                                FocusedPane::Sessions => FocusedPane::Events,
                                FocusedPane::Events => FocusedPane::Sessions,
                            };
                        }
                        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
                        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
                        KeyCode::Char('r') => {
                            let _ = app.refresh_sessions();
                        }
                        KeyCode::Char('f') => {
                            app.filter_input.clear();
                            app.mode = InputMode::FilterInput;
                            app.focus = FocusedPane::Events;
                            app.status = None;
                        }
                        KeyCode::Char('F') => {
                            app.filter = None;
                            app.status = None;
                        }
                        KeyCode::Char('x') => app.delete_selected(),
                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            let half = app.events_height / 2;
                            app.scroll_events_up(half.max(1));
                        }
                        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            app.scroll_events_down(app.events_height / 2);
                        }
                        KeyCode::Char('G') => app.events_scroll = 0,
                        _ => {}
                    },
                    InputMode::FilterInput => match key.code {
                        KeyCode::Enter => {
                            app.filter = if app.filter_input.is_empty() {
                                None
                            } else {
                                Some(app.filter_input.clone())
                            };
                            app.mode = InputMode::Normal;
                        }
                        KeyCode::Esc => {
                            app.mode = InputMode::Normal;
                        }
                        KeyCode::Backspace => {
                            app.filter_input.pop();
                        }
                        KeyCode::Char(c) => {
                            app.filter_input.push(c);
                        }
                        _ => {}
                    },
                },
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    ..
                }) => app.scroll_events_up(3),
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    ..
                }) => app.scroll_events_down(3),
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row,
                    ..
                }) => {
                    if is_inside(app.sessions_area, column, row) {
                        app.focus = FocusedPane::Sessions;
                    } else if is_inside(app.events_area, column, row) {
                        app.focus = FocusedPane::Events;
                    }
                }
                _ => {}
            }
        }

        if app.quit {
            return Ok(());
        }

        app.sync_tail();
        app.poll_events();
    }
}

// ── Rendering ────────────────────────────────────────────────────────────

/// Render the two-pane layout: sessions (top) and events (bottom).
///
/// Each pane embeds keybinding hints in the bottom-right of its border.
/// The focused pane gets a normal border; the unfocused pane gets a dim one.
#[allow(clippy::too_many_lines, reason = "layout rendering in one function")]
fn draw(f: &mut ratatui::Frame, app: &mut App) {
    let [sessions_area, events_area] =
        Layout::vertical([Constraint::Percentage(35), Constraint::Fill(1)]).areas(f.area());

    // Store areas for mouse hit-testing.
    app.sessions_area = sessions_area;
    app.events_area = events_area;

    // Update events_height so sync_tail knows how many to load.
    app.events_height = events_area.height.saturating_sub(2) as usize;

    let theme = &app.theme;

    let sessions_border = if app.focus == FocusedPane::Sessions {
        theme.border_focused
    } else {
        theme.border_unfocused
    };
    let events_border = if app.focus == FocusedPane::Events {
        theme.border_focused
    } else {
        theme.border_unfocused
    };

    // -- Top pane: session list --
    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .map(|row| {
            let status = if row.alive { "●" } else { "○" };
            let row_style = if row.alive {
                theme.session_active
            } else {
                theme.session_dead
            };
            let ago = format_ago(row.info.started_at);
            let workspace = row
                .info
                .workspace
                .rsplit('/')
                .next()
                .unwrap_or(&row.info.workspace);
            let client = row.info.client_name.as_deref().unwrap_or("-");

            let mut lines = vec![Line::from(vec![
                Span::styled(format!("{status} "), row_style),
                Span::styled(
                    format!(
                        "{:<12} {:<20} {:<8} {}",
                        &row.info.id[..row.info.id.len().min(12)],
                        workspace,
                        client,
                        ago,
                    ),
                    row_style,
                ),
            ])];

            if !row.languages.is_empty() {
                let lang_str = row.languages.join(", ");
                lines.push(Line::styled(format!("    {lang_str}"), theme.session_meta));
            }

            ListItem::new(lines)
        })
        .collect();

    let sessions_hints = Line::from(vec![
        " j/k ".set_style(theme.hint_key),
        "navigate ".set_style(theme.hint_label),
        " r ".set_style(theme.hint_key),
        "refresh ".set_style(theme.hint_label),
        " x ".set_style(theme.hint_key),
        "delete log ".set_style(theme.hint_label),
        " q ".set_style(theme.hint_key),
        "quit ".set_style(theme.hint_label),
    ])
    .alignment(Alignment::Right);

    let sessions_block = Block::bordered()
        .title(" Sessions ".set_style(theme.title))
        .title_bottom(sessions_hints)
        .border_type(BorderType::Rounded)
        .border_style(sessions_border);

    let sessions_list = List::new(items)
        .block(sessions_block)
        .highlight_style(theme.selection)
        .highlight_symbol("▸ ");

    f.render_stateful_widget(sessions_list, sessions_area, &mut app.list_state);

    // -- Bottom pane: events --
    let filtered = app.visible_events();
    let total = filtered.len();
    let height = app.events_height;
    let scroll = app.events_scroll.min(total.saturating_sub(height));
    let end = total.saturating_sub(scroll);
    let start = end.saturating_sub(height);
    let visible_lines: Vec<Line> = filtered[start..end]
        .iter()
        .map(|ev| format_event_styled(ev, &app.icons, theme))
        .collect();
    drop(filtered);
    app.events_scroll = scroll;

    let title = build_events_title(app);
    let filter_suffix = app
        .filter
        .as_ref()
        .map_or(String::new(), |pat| format!(" [filter: {pat}]"));
    let above = start;
    let above_suffix = if above > 0 {
        format!(" [+{above}]")
    } else {
        String::new()
    };
    let full_title = format!("{title}{filter_suffix}{above_suffix}");

    let below = scroll;
    let mut bottom_spans: Vec<Span> = Vec::new();
    if below > 0 {
        bottom_spans.push(format!(" [+{below}] ").set_style(theme.hint_label));
    }
    match app.mode {
        InputMode::Normal => {
            bottom_spans.extend([
                " ^u/^d ".set_style(theme.hint_key),
                "scroll ".set_style(theme.hint_label),
                " G ".set_style(theme.hint_key),
                "latest ".set_style(theme.hint_label),
                " f ".set_style(theme.hint_key),
                "filter ".set_style(theme.hint_label),
                " F ".set_style(theme.hint_key),
                "clear ".set_style(theme.hint_label),
            ]);
        }
        InputMode::FilterInput => {
            bottom_spans.extend([
                " Filter: ".into(),
                format!("{}▏", app.filter_input).set_style(theme.hint_key),
                "  ".into(),
                " Enter ".set_style(theme.hint_key),
                "apply ".set_style(theme.hint_label),
                " Esc ".set_style(theme.hint_key),
                "cancel ".set_style(theme.hint_label),
            ]);
        }
    }
    let events_hints = Line::from(bottom_spans).alignment(Alignment::Right);

    let events_block = Block::bordered()
        .title(full_title.set_style(theme.title))
        .title_bottom(events_hints)
        .border_type(BorderType::Rounded)
        .border_style(events_border);
    let events_paragraph = Paragraph::new(visible_lines).block(events_block);

    f.render_widget(events_paragraph, events_area);

    // -- Scrollbar on events pane --
    if total > height {
        // ScrollbarState position is "distance from top".
        let scrollbar_pos = start;
        let mut scrollbar_state =
            ScrollbarState::new(total.saturating_sub(height)).position(scrollbar_pos);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
        // Render inside the events border (inset by 1 on each side).
        let scrollbar_area = Rect {
            x: events_area.x,
            y: events_area.y + 1,
            width: events_area.width,
            height: events_area.height.saturating_sub(2),
        };
        f.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }
}

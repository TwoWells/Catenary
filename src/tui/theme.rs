// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Theme, icon set, and event formatting helpers for the TUI.
//!
//! All colors use the terminal's ANSI palette so the TUI automatically
//! inherits whatever theme the user has configured.

use std::time::{Duration, Instant};

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
    ///
    /// Uses `REVERSED` for selection highlight. Prefer [`Theme::detect()`]
    /// at runtime to derive a subtler background-shifted highlight color.
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

    /// Build a theme by querying the terminal's background color.
    ///
    /// Sends an OSC 11 query to detect the terminal background, then derives
    /// a subtle selection highlight by shifting the lightness in HSL space
    /// (+0.2 for dark backgrounds, −0.2 for light). Falls back to `REVERSED`
    /// if the terminal doesn't respond or the query fails.
    #[must_use]
    pub fn detect() -> Self {
        let mut theme = Self::new();
        if let Some((r, g, b)) = detect_terminal_bg() {
            theme.selection = Style::new().bg(selection_bg_from_terminal(r, g, b));
        }
        theme
    }
}

// ── Terminal background detection ────────────────────────────────────────

/// Lightness shift applied to the terminal background for the selection
/// highlight. Positive for dark backgrounds, negative for light.
const SELECTION_LIGHTNESS_SHIFT: f64 = 0.2;

/// Poll interval for failure detection sampling (matches `wait.rs`).
const DETECTION_POLL: Duration = Duration::from_millis(200);

/// CPU-tick threshold before giving up on the terminal response.
///
/// 10 ticks = 100ms of actual CPU time. Generous for a one-shot query
/// that completes in <10ms under normal conditions.
const DETECTION_TICK_THRESHOLD: i64 = 10;

/// Wall-clock safety cap for pathological cases (D-state, NFS hang).
const DETECTION_WALL_CAP: Duration = Duration::from_secs(2);

/// Query the terminal's background color via OSC 11.
///
/// Uses load-aware failure detection (same pattern as `load_aware_grace`
/// in `src/lsp/wait.rs`) instead of a fixed wall-clock timeout: keeps
/// waiting while the system is under load and our process is sleeping,
/// bails only when real CPU time has been burned without a response.
///
/// Returns `Some((r, g, b))` with 8-bit RGB values if the terminal
/// responds, or `None` if the query fails or the threshold is exhausted.
#[cfg(unix)]
fn detect_terminal_bg() -> Option<(u8, u8, u8)> {
    use std::io::{Read, Write};
    use std::sync::mpsc::RecvTimeoutError;

    use catenary_proc::ProcessMonitor;

    // Open /dev/tty directly to avoid contention with crossterm's stdin.
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;

    // We need raw mode for character-at-a-time reads.
    let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
    if !was_raw {
        crossterm::terminal::enable_raw_mode().ok()?;
    }

    // Send OSC 11 query: "what is the background color?"
    tty.write_all(b"\x1b]11;?\x07").ok()?;
    tty.flush().ok()?;

    // Read the response in a background thread.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::with_capacity(64);
        let mut byte = [0u8; 1];
        loop {
            match tty.read(&mut byte) {
                Ok(1) => {
                    buf.push(byte[0]);
                    // Terminators: BEL (\x07) or ST (\x1b\\).
                    if byte[0] == 0x07 {
                        let _ = tx.send(buf);
                        return;
                    }
                    if buf.len() >= 2 && buf[buf.len() - 2] == 0x1b && buf[buf.len() - 1] == b'\\' {
                        let _ = tx.send(buf);
                        return;
                    }
                }
                _ => return,
            }
        }
    });

    // Load-aware wait: poll for the response, sampling our own process to
    // distinguish "system is loaded" (sleeping, ticks flat → keep waiting)
    // from "terminal won't respond" (CPU budget exhausted → give up).
    let mut monitor = ProcessMonitor::new(std::process::id())?;
    let deadline = Instant::now() + DETECTION_WALL_CAP;
    let mut remaining_threshold = DETECTION_TICK_THRESHOLD;

    let result = loop {
        match rx.recv_timeout(DETECTION_POLL) {
            Ok(response) => break Some(response),
            Err(RecvTimeoutError::Disconnected) => break None,
            Err(RecvTimeoutError::Timeout) => {}
        }

        let (delta, state) = monitor.sample()?;
        if state == catenary_proc::ProcessState::Dead {
            break None;
        }
        // Only drain threshold on unexplained CPU work: Running + advancing ticks.
        if state == catenary_proc::ProcessState::Running && delta > 0 {
            remaining_threshold -= i64::try_from(delta).unwrap_or(remaining_threshold);
        }

        if remaining_threshold <= 0 || Instant::now() >= deadline {
            break None;
        }
    };

    if !was_raw {
        let _ = crossterm::terminal::disable_raw_mode();
    }

    result.and_then(|r| parse_osc11_response(&r))
}

/// Non-Unix fallback: always returns `None`.
#[cfg(not(unix))]
const fn detect_terminal_bg() -> Option<(u8, u8, u8)> {
    None
}

/// Parse an OSC 11 response into 8-bit RGB.
///
/// Expected format: `\x1b]11;rgb:RRRR/GGGG/BBBB<terminator>`
/// where each channel is 1–4 hex digits. We take the high byte of each
/// 16-bit value (i.e., for `1a1a` we return `0x1a`).
fn parse_osc11_response(response: &[u8]) -> Option<(u8, u8, u8)> {
    let text = std::str::from_utf8(response).ok()?;

    // Find "rgb:" and extract the color portion.
    let rgb_start = text.find("rgb:")?;
    let rgb_part = &text[rgb_start + 4..];

    // Strip terminator characters from the end.
    let rgb_clean = rgb_part.trim_end_matches(['\x07', '\\', '\x1b']);

    let mut channels = rgb_clean.splitn(3, '/');
    let r_hex = channels.next()?;
    let g_hex = channels.next()?;
    let b_hex = channels.next()?;

    Some((
        parse_osc_channel(r_hex)?,
        parse_osc_channel(g_hex)?,
        parse_osc_channel(b_hex)?,
    ))
}

/// Parse a single OSC color channel (1–4 hex digits) into an 8-bit value.
///
/// Terminals may report 4, 2, or even 1 hex digit(s) per channel.
/// For 4 digits (16-bit), we take the high byte. For 2, use directly.
/// For 1, scale up.
fn parse_osc_channel(hex: &str) -> Option<u8> {
    let val = u16::from_str_radix(hex, 16).ok()?;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "intentional 16-to-8-bit conversion"
    )]
    let byte = match hex.len() {
        4 => (val >> 8) as u8,
        3 => (val >> 4) as u8,
        2 => val as u8,
        1 => (val * 17) as u8, // 0x0 → 0x00, 0xf → 0xff
        _ => return None,
    };
    Some(byte)
}

// ── HSL color math ───────────────────────────────────────────────────────

/// Convert 8-bit RGB to HSL.
///
/// Returns `(hue, sat, light)` where `hue` is in `[0, 360)`, `sat` and
/// `light` in `[0, 1]`.
#[allow(
    clippy::many_single_char_names,
    reason = "r/g/b/h/s/l are standard color math notation"
)]
fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let rf = f64::from(r) / 255.0;
    let gf = f64::from(g) / 255.0;
    let bf = f64::from(b) / 255.0;

    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let delta = max - min;

    let light = f64::midpoint(max, min);

    if delta < f64::EPSILON {
        return (0.0, 0.0, light);
    }

    let sat = if light <= 0.5 {
        delta / (max + min)
    } else {
        delta / (2.0 - max - min)
    };

    let hue_sector = if (max - rf).abs() < f64::EPSILON {
        ((gf - bf) / delta) % 6.0
    } else if (max - gf).abs() < f64::EPSILON {
        (bf - rf) / delta + 2.0
    } else {
        (rf - gf) / delta + 4.0
    };

    let hue = hue_sector * 60.0;
    let hue = if hue < 0.0 { hue + 360.0 } else { hue };

    (hue, sat, light)
}

/// Convert HSL to 8-bit RGB.
///
/// `hue` is in `[0, 360)`, `sat` and `light` in `[0, 1]`.
#[allow(
    clippy::many_single_char_names,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "standard HSL math with clamped f64 to u8 conversion"
)]
fn hsl_to_rgb(hue: f64, sat: f64, light: f64) -> (u8, u8, u8) {
    if sat < f64::EPSILON {
        let val = (light * 255.0).round() as u8;
        return (val, val, val);
    }

    let q_val = if light < 0.5 {
        light * (1.0 + sat)
    } else {
        light.mul_add(-sat, light + sat)
    };
    let p_val = 2.0f64.mul_add(light, -q_val);
    let h_norm = hue / 360.0;

    let channel = |tc: f64| -> u8 {
        let tc = if tc < 0.0 {
            tc + 1.0
        } else if tc > 1.0 {
            tc - 1.0
        } else {
            tc
        };
        let out = if tc < 1.0 / 6.0 {
            ((q_val - p_val) * 6.0).mul_add(tc, p_val)
        } else if tc < 0.5 {
            q_val
        } else if tc < 2.0 / 3.0 {
            ((q_val - p_val) * (2.0 / 3.0 - tc)).mul_add(6.0, p_val)
        } else {
            p_val
        };
        (out * 255.0).round() as u8
    };

    (
        channel(h_norm + 1.0 / 3.0),
        channel(h_norm),
        channel(h_norm - 1.0 / 3.0),
    )
}

/// Derive a selection background color from the terminal's background.
///
/// Shifts lightness in HSL space: +0.2 for dark backgrounds, −0.2 for light.
#[allow(
    clippy::many_single_char_names,
    reason = "r/g/b are standard color notation"
)]
fn selection_bg_from_terminal(r: u8, g: u8, b: u8) -> Color {
    let (hue, sat, light) = rgb_to_hsl(r, g, b);
    let new_light = if light < 0.5 {
        (light + SELECTION_LIGHTNESS_SHIFT).min(1.0)
    } else {
        (light - SELECTION_LIGHTNESS_SHIFT).max(0.0)
    };
    let (nr, ng, nb) = hsl_to_rgb(hue, sat, new_light);
    Color::Rgb(nr, ng, nb)
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
    /// Glob tool icon.
    pub tool_glob: String,
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
            tool_glob: config.tool_glob.unwrap_or_else(|| base.7.to_string()),
            tool_default: config.tool_default.unwrap_or_else(|| base.8.to_string()),
        }
    }

    /// Returns `(unicode_defaults, nerd_defaults)` tuples.
    ///
    /// Order: `diag_error`, `diag_warn`, `diag_info`, `diag_ok`, `lock`, `unlock`,
    ///        `tool_search`, `tool_glob`, `tool_default`.
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
        ),
    ) {
        let unicode = (
            "\u{2717} ", // ✗
            "\u{26A0} ", // ⚠
            "\u{2139} ", // ℹ
            "\u{2713} ", // ✓
            "\u{25B6} ", // ▶
            "\u{25C0} ", // ◀
            "\u{2192} ", // → (search)
            "\u{2192} ", // → (glob)
            "\u{2192} ", // → (default)
        );
        let nerd = (
            " ",         // nf-cod-error
            " ",         // nf-cod-warning
            " ",         // nf-cod-info
            " ",         // nf-cod-check
            " ",         // nf-cod-lock
            " ",         // nf-cod-unlock
            " ",         // nf-cod-search
            " ",         // nf-cod-file_directory
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
        EventKind::ToolResult {
            tool,
            success,
            output: _,
            ..
        } => {
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
        "grep" => &icons.tool_search,
        "glob" => &icons.tool_glob,
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
            output: _,
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
            tool: "grep".to_string(),
            file: Some("/src/main.rs".to_string()),
        });
        let plain = format_event_plain(&ev);
        assert!(plain.contains("grep"));
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

    // ── HSL color math tests ─────────────────────────────────────────────

    #[test]
    fn test_rgb_to_hsl_black() {
        let (h, s, l) = rgb_to_hsl(0, 0, 0);
        assert!((h - 0.0).abs() < 0.01);
        assert!((s - 0.0).abs() < 0.01);
        assert!((l - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_rgb_to_hsl_white() {
        let (h, s, l) = rgb_to_hsl(255, 255, 255);
        assert!((h - 0.0).abs() < 0.01);
        assert!((s - 0.0).abs() < 0.01);
        assert!((l - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_rgb_to_hsl_pure_red() {
        let (h, s, l) = rgb_to_hsl(255, 0, 0);
        assert!((h - 0.0).abs() < 0.01);
        assert!((s - 1.0).abs() < 0.01);
        assert!((l - 0.5).abs() < 0.01);
    }

    #[test]
    #[allow(
        clippy::many_single_char_names,
        reason = "r/g/b/h/s/l are standard color math notation"
    )]
    fn test_hsl_roundtrip_dark_gray() {
        let (r, g, b) = (30u8, 30, 30);
        let (h, s, l) = rgb_to_hsl(r, g, b);
        let (r2, g2, b2) = hsl_to_rgb(h, s, l);
        assert!((i16::from(r) - i16::from(r2)).abs() <= 1);
        assert!((i16::from(g) - i16::from(g2)).abs() <= 1);
        assert!((i16::from(b) - i16::from(b2)).abs() <= 1);
    }

    #[test]
    #[allow(
        clippy::many_single_char_names,
        reason = "r/g/b/h/s/l are standard color math notation"
    )]
    fn test_hsl_roundtrip_color() {
        // Teal-ish: rgb(50, 130, 180)
        let (r, g, b) = (50u8, 130, 180);
        let (h, s, l) = rgb_to_hsl(r, g, b);
        let (r2, g2, b2) = hsl_to_rgb(h, s, l);
        assert!((i16::from(r) - i16::from(r2)).abs() <= 1);
        assert!((i16::from(g) - i16::from(g2)).abs() <= 1);
        assert!((i16::from(b) - i16::from(b2)).abs() <= 1);
    }

    #[test]
    fn test_selection_bg_dark_background_lightens() {
        // Typical dark terminal: rgb(26, 26, 26), L ≈ 0.10
        let Color::Rgb(r, g, b) = selection_bg_from_terminal(26, 26, 26) else {
            unreachable!("selection_bg_from_terminal always returns Color::Rgb");
        };
        // Should be noticeably lighter.
        assert!(r > 26, "red channel should increase: {r}");
        assert!(g > 26, "green channel should increase: {g}");
        assert!(b > 26, "blue channel should increase: {b}");
    }

    #[test]
    fn test_selection_bg_light_background_darkens() {
        // Light terminal: rgb(240, 240, 240), L ≈ 0.94
        let Color::Rgb(r, g, b) = selection_bg_from_terminal(240, 240, 240) else {
            unreachable!("selection_bg_from_terminal always returns Color::Rgb");
        };
        // Should be noticeably darker.
        assert!(r < 240, "red channel should decrease: {r}");
        assert!(g < 240, "green channel should decrease: {g}");
        assert!(b < 240, "blue channel should decrease: {b}");
    }

    #[test]
    fn test_selection_bg_preserves_hue() {
        // Blueish dark bg: rgb(20, 20, 40)
        let Color::Rgb(r, g, b) = selection_bg_from_terminal(20, 20, 40) else {
            unreachable!("selection_bg_from_terminal always returns Color::Rgb");
        };
        // Blue channel should still be the dominant one.
        assert!(b >= r, "blue should still dominate: r={r} b={b}");
        assert!(b >= g, "blue should still dominate: g={g} b={b}");
    }

    // ── OSC parsing tests ────────────────────────────────────────────────

    #[test]
    fn test_parse_osc11_response_4digit() {
        // Typical response: ESC ] 11 ; rgb:1a1a/1a1a/1a1a BEL
        let response = b"\x1b]11;rgb:1a1a/1a1a/1a1a\x07";
        let result = parse_osc11_response(response);
        assert_eq!(result, Some((0x1a, 0x1a, 0x1a)));
    }

    #[test]
    fn test_parse_osc11_response_2digit() {
        let response = b"\x1b]11;rgb:1a/1a/1a\x07";
        let result = parse_osc11_response(response);
        assert_eq!(result, Some((0x1a, 0x1a, 0x1a)));
    }

    #[test]
    fn test_parse_osc11_response_st_terminator() {
        // ST terminator: ESC \
        let response = b"\x1b]11;rgb:ffff/0000/8080\x1b\\";
        let result = parse_osc11_response(response);
        assert_eq!(result, Some((0xff, 0x00, 0x80)));
    }

    #[test]
    fn test_parse_osc11_garbage() {
        let response = b"not a valid response";
        assert!(parse_osc11_response(response).is_none());
    }

    #[test]
    fn test_parse_osc_channel_variants() {
        assert_eq!(parse_osc_channel("ff"), Some(0xff));
        assert_eq!(parse_osc_channel("ffff"), Some(0xff));
        assert_eq!(parse_osc_channel("0000"), Some(0x00));
        assert_eq!(parse_osc_channel("8080"), Some(0x80));
        assert_eq!(parse_osc_channel("f"), Some(0xff));
        assert_eq!(parse_osc_channel("0"), Some(0x00));
    }

    #[test]
    fn test_theme_detect_fallback() {
        // Theme::detect() should not panic even if the terminal doesn't
        // support OSC 11 (e.g., in CI). It falls back to Theme::new().
        let theme = Theme::detect();
        // Selection style should be set (either Rgb bg or REVERSED fallback).
        let _ = theme.selection;
    }
}

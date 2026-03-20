// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Responsive degradation chains for narrow and short terminals.
//!
//! When the terminal is too narrow or too short for the full UI, this module
//! computes progressive degradation: Sessions tree shrinking, Events panel
//! title truncation, navigation hint dropping, and the absolute minimum
//! size (5 columns × 2 lines, below which the screen fills with `╳`).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use unicode_width::UnicodeWidthStr;

// ── Constants ───────────────────────────────────────────────────────────

/// Minimum useful workspace tail length (in chars) for title level 1.
const MIN_WORKSPACE_TAIL: usize = 5;

// ── Types ───────────────────────────────────────────────────────────────

/// Terminal size classification for degradation decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeClass {
    /// Full featured — no degradation needed.
    Full,
    /// Comfortable — minor adjustments.
    Comfortable,
    /// Tight — significant trimming.
    Tight,
    /// Cramped — minimal chrome.
    Cramped,
    /// Extreme — bare content, no borders.
    Extreme,
    /// Below minimum — fill with ╳.
    BelowMinimum,
}

/// Computed degradation state for the current terminal size.
#[derive(Debug, Clone)]
pub struct DegradationState {
    /// Overall size classification.
    pub size_class: SizeClass,
    /// Whether the Sessions tree should be visible.
    pub sessions_visible: bool,
    /// Width for the Sessions tree (0 if hidden).
    pub sessions_width: u16,
    /// How many hint levels to drop (0 = all hints visible).
    pub hint_drop_level: u8,
    /// Title degradation level (0 = full, 6 = extreme).
    pub title_level: u8,
    /// Whether panel borders should be rendered.
    pub show_borders: bool,
    /// Whether language server info fits in titles.
    pub show_ls_info: bool,
}

/// Configuration for degradation thresholds.
pub struct DegradationConfig {
    /// Preferred Sessions width as fraction of terminal (e.g., 0.4).
    pub sessions_width_ratio: f64,
    /// Minimum Sessions width before collapsing.
    pub sessions_min_width: u16,
    /// Minimum panel width for Events panels.
    pub panel_min_width: u16,
    /// Minimum panel height for Events panels.
    pub panel_min_height: u16,
}

impl Default for DegradationConfig {
    fn default() -> Self {
        Self {
            sessions_width_ratio: 0.4,
            sessions_min_width: 20,
            panel_min_width: 20,
            panel_min_height: 4,
        }
    }
}

// ── Minimum size ────────────────────────────────────────────────────────

/// Absolute minimum terminal size: 5 columns × 2 lines.
///
/// Returns true if the terminal is below this threshold.
#[must_use]
pub const fn is_below_minimum(width: u16, height: u16) -> bool {
    width < 5 || height < 2
}

/// Fill the entire area with `╳` characters.
///
/// Used when the terminal is below the minimum usable size.
pub fn render_below_minimum(area: Rect, buf: &mut Buffer) {
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char('\u{2573}'); // ╳
            }
        }
    }
}

// ── Degradation computation ─────────────────────────────────────────────

/// Compute the full degradation state for the current terminal size.
///
/// Given terminal dimensions and panel count, determines Sessions tree
/// visibility and width, hint drop level, title degradation level, and
/// whether borders and language server info should be shown.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "terminal dimensions are bounded by u16"
)]
pub fn compute_degradation(
    width: u16,
    height: u16,
    panel_count: usize,
    config: &DegradationConfig,
) -> DegradationState {
    if is_below_minimum(width, height) {
        return DegradationState {
            size_class: SizeClass::BelowMinimum,
            sessions_visible: false,
            sessions_width: 0,
            hint_drop_level: 7,
            title_level: 6,
            show_borders: false,
            show_ls_info: false,
        };
    }

    let panels_per_row = if panel_count == 0 {
        1
    } else {
        isqrt_ceil(panel_count)
    };

    let ideal_sessions = (f64::from(width) * config.sessions_width_ratio) as u16;

    let panels_per_row_u16 = (panels_per_row as u16).max(1);
    let min_events_width = config.panel_min_width.saturating_mul(panels_per_row_u16);

    // Determine sessions visibility and width.
    let (sessions_visible, sessions_width) = if panel_count == 0 {
        (true, width)
    } else {
        let events_with_ideal = width.saturating_sub(ideal_sessions);
        if events_with_ideal >= min_events_width {
            (true, ideal_sessions)
        } else {
            let available_for_sessions = width.saturating_sub(min_events_width);
            if available_for_sessions >= config.sessions_min_width {
                (true, available_for_sessions)
            } else {
                (false, 0)
            }
        }
    };

    let events_width = width.saturating_sub(sessions_width);
    let per_panel_width = if panels_per_row > 0 && events_width > 0 {
        events_width / panels_per_row_u16
    } else {
        width
    };

    // Per-panel height (balanced grid assumption).
    let rows = if panel_count > 0 {
        panel_count.div_ceil(panels_per_row).max(1) as u16
    } else {
        1
    };
    let available_height = height.saturating_sub(1); // bottom border/hints row
    let per_panel_height = if rows > 0 {
        available_height / rows
    } else {
        available_height
    };

    // Hint drop level spans full terminal width.
    let hint_avail = width.saturating_sub(6); // chrome: ──┤ ... ├──┘
    let hint_drop_level = compute_hint_drop_level(hint_avail);

    let title_level = compute_title_level(per_panel_width);
    let show_borders = per_panel_height >= config.panel_min_height;
    let show_ls_info = per_panel_width >= 30;

    let size_class = if hint_drop_level == 0 && show_borders && show_ls_info && sessions_visible {
        SizeClass::Full
    } else if show_borders && sessions_visible {
        SizeClass::Comfortable
    } else if show_borders {
        SizeClass::Tight
    } else if per_panel_height >= 2 {
        SizeClass::Cramped
    } else {
        SizeClass::Extreme
    };

    DegradationState {
        size_class,
        sessions_visible,
        sessions_width,
        hint_drop_level,
        title_level,
        show_borders,
        show_ls_info,
    }
}

/// Integer square root rounded up (pure integer math).
const fn isqrt_ceil(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let s = n.isqrt();
    if s * s == n { s } else { s + 1 }
}

/// Compute hint drop level from available hint-area width.
///
/// | Level | Meaning                    | Min width |
/// |-------|----------------------------|-----------|
/// |   0   | All hints with separators  | 25        |
/// |   1   | All hints, spaces only     | 17        |
/// |   2   | Drop `z ═`                 | 13        |
/// |   3   | Drop `v ▒`                 |  9        |
/// |   4   | Drop `f ░`                 |  5        |
/// |   5   | Drop `?`                   |  3        |
/// |   6   | Border only                |  0        |
const fn compute_hint_drop_level(available: u16) -> u8 {
    if available >= 25 {
        0
    } else if available >= 17 {
        1
    } else if available >= 13 {
        2
    } else if available >= 9 {
        3
    } else if available >= 5 {
        4
    } else if available >= 3 {
        5
    } else {
        6
    }
}

/// Compute title degradation level from per-panel width.
const fn compute_title_level(per_panel_width: u16) -> u8 {
    if per_panel_width >= 35 {
        0
    } else if per_panel_width >= 25 {
        1
    } else if per_panel_width >= 17 {
        2
    } else if per_panel_width >= 14 {
        3
    } else if per_panel_width >= 7 {
        4
    } else if per_panel_width >= 2 {
        5
    } else {
        6
    }
}

// ── Title degradation ───────────────────────────────────────────────────

/// Progressively truncate the panel title to fit the available width.
///
/// Computes the appropriate degradation level from `max_width`:
/// ```text
/// Level 0: "Events [029ba740 ~/Projects/Catenary]"
/// Level 1: "Events [029ba740 …ects/Catenary]"
/// Level 2: "Events [029ba740]"
/// Level 3: "Events [029b…]"
/// Level 4: "[029b…]"
/// Level 5: "029b…"
/// Level 6: "0…"
/// ```
#[must_use]
pub fn degrade_title(session_id: &str, workspace: Option<&str>, max_width: u16) -> String {
    let max = max_width as usize;
    let id8 = if session_id.len() > 8 {
        &session_id[..8]
    } else {
        session_id
    };

    // Level 0: full context with workspace.
    if let Some(ws) = workspace {
        let full = format!("Events [{id8} {ws}]");
        if UnicodeWidthStr::width(full.as_str()) <= max {
            return full;
        }

        // Level 1: truncated workspace — keep the last N chars.
        let prefix = format!("Events [{id8} \u{2026}");
        let suffix = "]";
        let overhead = UnicodeWidthStr::width(prefix.as_str()) + UnicodeWidthStr::width(suffix);
        let budget = max.saturating_sub(overhead);
        if budget >= MIN_WORKSPACE_TAIL {
            let ws_chars: Vec<char> = ws.chars().collect();
            let start = ws_chars.len().saturating_sub(budget);
            let ws_tail: String = ws_chars[start..].iter().collect();
            let candidate = format!("{prefix}{ws_tail}{suffix}");
            if UnicodeWidthStr::width(candidate.as_str()) <= max {
                return candidate;
            }
        }
    }

    // Level 2: session ID only.
    let level2 = format!("Events [{id8}]");
    if UnicodeWidthStr::width(level2.as_str()) <= max {
        return level2;
    }

    // Level 3: truncated session ID (first 4 chars).
    let id4 = if session_id.len() > 4 {
        &session_id[..4]
    } else {
        session_id
    };
    let level3 = format!("Events [{id4}\u{2026}]");
    if UnicodeWidthStr::width(level3.as_str()) <= max {
        return level3;
    }

    // Level 4: drop "Events" prefix.
    let level4 = format!("[{id4}\u{2026}]");
    if UnicodeWidthStr::width(level4.as_str()) <= max {
        return level4;
    }

    // Levels 5–6: bare ID prefix with ellipsis.
    if max >= 2 {
        let id_budget = max - 1; // 1 column for …
        let id_prefix: String = session_id.chars().take(id_budget).collect();
        return format!("{id_prefix}\u{2026}");
    }

    if max == 1 {
        return session_id
            .chars()
            .next()
            .map(String::from)
            .unwrap_or_default();
    }

    String::new()
}

// ── Hint degradation ────────────────────────────────────────────────────

/// All navigation hints in display order.
const ALL_HINTS: [(&str, &str); 5] = [
    ("z", "\u{2550}"), // ═
    ("v", "\u{2592}"), // ▒
    ("f", "\u{2591}"), // ░
    ("?", ""),
    ("q", "\u{2718}"), // ✘
];

/// Display width of a single hint: `key symbol` or just `key` if symbol is empty.
fn hint_display_width(key: &str, symbol: &str) -> usize {
    if symbol.is_empty() {
        UnicodeWidthStr::width(key)
    } else {
        UnicodeWidthStr::width(key) + 1 + UnicodeWidthStr::width(symbol)
    }
}

/// Total display width of hints joined by ` ╱ ` separators.
fn hints_width_with_separators(hints: &[(&str, &str)]) -> usize {
    if hints.is_empty() {
        return 0;
    }
    let content: usize = hints.iter().map(|(k, s)| hint_display_width(k, s)).sum();
    let seps = (hints.len() - 1) * 3; // " ╱ " = 3 columns
    content + seps
}

/// Total display width of hints joined by single spaces.
fn hints_width_spaced(hints: &[(&str, &str)]) -> usize {
    if hints.is_empty() {
        return 0;
    }
    let content: usize = hints.iter().map(|(k, s)| hint_display_width(k, s)).sum();
    content + hints.len() - 1
}

/// Return the navigation hints that fit in the given width.
///
/// Degradation order:
/// 1. All 5 hints with `╱` separators.
/// 2. All 5 hints, space-separated.
/// 3. Drop `z ═`.
/// 4. Drop `v ▒`.
/// 5. Drop `f ░`.
/// 6. Drop `?`.
/// 7. Drop `q ✘`.
/// 8. Empty (border only).
#[must_use]
pub fn degrade_hints(max_width: u16) -> Vec<(&'static str, &'static str)> {
    let max = max_width as usize;
    let all = ALL_HINTS.to_vec();

    // Level 1: all hints with separators.
    if hints_width_with_separators(&all) <= max {
        return all;
    }

    // Level 2: all hints, space-separated.
    if hints_width_spaced(&all) <= max {
        return all;
    }

    // Levels 3–7: progressively drop hints from the front.
    for drop_count in 1..=4 {
        let remaining = ALL_HINTS[drop_count..].to_vec();
        if hints_width_spaced(&remaining) <= max {
            return remaining;
        }
    }

    // Level 8: empty.
    Vec::new()
}

// ── Sessions path degradation ───────────────────────────────────────────

/// Progressively truncate a workspace path to fit the available width.
///
/// Smooth character-by-character erosion:
/// ```text
/// ~/Projects/Catenary       ← full
/// …Projects/Catenary        ← erode from left (keeps path context)
/// …rojects/Catenary
/// …s/Catenary
/// …/Catenary                ← last left-erosion step (still has /)
/// Catenary                  ← basename only
/// Catena…                   ← erode basename from right
/// Cat…
/// C…
/// C                         ← single char
/// ```
#[must_use]
pub fn degrade_sessions_path(path: &str, max_width: u16) -> String {
    let max = max_width as usize;

    // Full path fits.
    if UnicodeWidthStr::width(path) <= max {
        return path.to_string();
    }

    // Erode from the left: …<tail>, dropping more chars each step.
    // Stop when the tail no longer contains `/` — beyond that the `…`
    // prefix is confusing (e.g., `…atenary`).
    let path_chars: Vec<char> = path.chars().collect();
    for drop in 2..path_chars.len() {
        let tail: String = path_chars[drop..].iter().collect();
        if !tail.contains('/') {
            break;
        }
        let candidate = format!("\u{2026}{tail}");
        if UnicodeWidthStr::width(candidate.as_str()) <= max {
            return candidate;
        }
    }

    // Basename only.
    let basename = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path);

    if UnicodeWidthStr::width(basename) <= max {
        return basename.to_string();
    }

    // Erode basename from the right: prefix…
    let base_chars: Vec<char> = basename.chars().collect();
    if max >= 2 {
        let keep = max - 1; // 1 column for …
        let prefix: String = base_chars.iter().take(keep).collect();
        return format!("{prefix}\u{2026}");
    }

    if max == 1 {
        return base_chars.first().map_or_else(String::new, char::to_string);
    }

    String::new()
}

// ── Language server title degradation ───────────────────────────────────

/// Build a degraded language server info string for the title bar.
///
/// Degradation levels:
/// 1. Full names with status icons: `● rust ╱ ● ts`
/// 2. Names with separators, no icons: `rust ╱ ts`
/// 3. Names space-separated: `rust ts`
/// 4. Empty string (not enough room).
#[must_use]
pub fn degrade_ls_title(servers: &[String], max_width: u16) -> String {
    if servers.is_empty() {
        return String::new();
    }

    let max = max_width as usize;

    // Level 1: icons + fancy separators.
    let full = build_ls_string(servers, true, true);
    if UnicodeWidthStr::width(full.as_str()) <= max {
        return full;
    }

    // Level 2: no icons, fancy separators.
    let no_icons = build_ls_string(servers, false, true);
    if UnicodeWidthStr::width(no_icons.as_str()) <= max {
        return no_icons;
    }

    // Level 3: no icons, space-separated.
    let no_seps = build_ls_string(servers, false, false);
    if UnicodeWidthStr::width(no_seps.as_str()) <= max {
        return no_seps;
    }

    // Level 4: empty.
    String::new()
}

/// Build a language server info string with configurable icons and separators.
fn build_ls_string(servers: &[String], icons: bool, fancy_sep: bool) -> String {
    let mut result = String::new();
    for (i, name) in servers.iter().enumerate() {
        if i > 0 {
            if fancy_sep {
                result.push_str(" \u{2571} "); // ╱
            } else {
                result.push(' ');
            }
        }
        if icons {
            result.push_str("\u{25CF} "); // ● + space
        }
        result.push_str(name);
    }
    result
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn test_below_minimum() {
        assert!(is_below_minimum(4, 1));
        assert!(is_below_minimum(3, 2));
        assert!(is_below_minimum(5, 1));
        assert!(!is_below_minimum(5, 2));
        assert!(!is_below_minimum(100, 50));
    }

    #[test]
    fn test_render_below_minimum() {
        let backend = TestBackend::new(3, 1);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                render_below_minimum(f.area(), f.buffer_mut());
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
    fn test_full_size_no_degradation() {
        let config = DegradationConfig::default();
        let state = compute_degradation(200, 50, 2, &config);
        assert!(state.sessions_visible, "sessions should be visible");
        assert_eq!(state.hint_drop_level, 0, "no hint drops at full width");
        assert!(state.show_borders, "borders should be shown");
    }

    #[test]
    fn test_narrow_hides_sessions() {
        let config = DegradationConfig::default();
        let state = compute_degradation(40, 24, 2, &config);
        assert!(
            !state.sessions_visible,
            "sessions should be hidden at 40 wide with 2 panels"
        );
    }

    #[test]
    fn test_title_degradation_full() {
        let result = degrade_title("029ba740", Some("~/Projects/Catenary"), 50);
        assert!(
            result.contains("029ba740"),
            "full title should contain session ID: {result}"
        );
        assert!(
            result.contains("~/Projects/Catenary"),
            "full title should contain workspace: {result}"
        );
    }

    #[test]
    fn test_title_degradation_drop_workspace() {
        let result = degrade_title("029ba740", Some("~/Projects/Catenary"), 20);
        assert!(
            result.contains("029ba740"),
            "title should contain session ID: {result}"
        );
        assert!(
            !result.contains("Catenary"),
            "title should not contain workspace at width 20: {result}"
        );
    }

    #[test]
    fn test_title_degradation_truncate_id() {
        let result = degrade_title("029ba740", Some("~/Projects/Catenary"), 12);
        assert!(
            result.contains('\u{2026}'),
            "truncated title should contain ellipsis: {result}"
        );
        assert!(
            result.contains("029b"),
            "truncated title should contain ID prefix: {result}"
        );
    }

    #[test]
    fn test_title_degradation_extreme() {
        let result = degrade_title("029ba740", Some("~/Projects/Catenary"), 3);
        assert!(
            result.contains('\u{2026}'),
            "extreme title should contain ellipsis: {result}"
        );
        assert!(
            UnicodeWidthStr::width(result.as_str()) <= 3,
            "extreme title width should be \u{2264} 3: {result}"
        );
    }

    #[test]
    fn test_hints_full_width() {
        let hints = degrade_hints(60);
        assert_eq!(hints.len(), 5, "all 5 hints should be present at width 60");
        assert_eq!(hints[0], ("z", "\u{2550}"));
        assert_eq!(hints[4], ("q", "\u{2718}"));
    }

    #[test]
    fn test_hints_narrow() {
        let hints = degrade_hints(15);
        assert!(
            hints.len() < 5,
            "fewer than 5 hints at width 15, got {}",
            hints.len()
        );
        assert!(
            !hints.is_empty(),
            "should still have some hints at width 15"
        );
    }

    #[test]
    fn test_hints_minimal() {
        let hints = degrade_hints(2);
        assert!(hints.is_empty(), "no hints should fit at width 2");
    }

    #[test]
    fn test_sessions_path_full() {
        let result = degrade_sessions_path("~/Projects/Catenary", 30);
        assert_eq!(result, "~/Projects/Catenary");
    }

    #[test]
    fn test_sessions_path_truncated() {
        // Width 12: left-erosion should produce …ts/Catenary (12 cols).
        let result = degrade_sessions_path("~/Projects/Catenary", 12);
        assert_eq!(result, "\u{2026}ts/Catenary");
    }

    #[test]
    fn test_sessions_path_erosion_chain() {
        // Verify smooth left-erosion at each width.
        assert_eq!(
            degrade_sessions_path("~/Projects/Catenary", 18),
            "\u{2026}Projects/Catenary"
        );
        assert_eq!(
            degrade_sessions_path("~/Projects/Catenary", 14),
            "\u{2026}ects/Catenary"
        );
        assert_eq!(
            degrade_sessions_path("~/Projects/Catenary", 10),
            "\u{2026}/Catenary"
        );
        // Width 9: …/Catenary (10) doesn't fit, basename Catenary (8) does.
        assert_eq!(degrade_sessions_path("~/Projects/Catenary", 9), "Catenary");
        // Width 7: basename doesn't fit, erode from right.
        assert_eq!(
            degrade_sessions_path("~/Projects/Catenary", 7),
            "Catena\u{2026}"
        );
    }

    #[test]
    fn test_sessions_path_minimal() {
        let result = degrade_sessions_path("~/Projects/Catenary", 5);
        assert!(
            UnicodeWidthStr::width(result.as_str()) <= 5,
            "minimal path should be \u{2264} 5 columns: {result}"
        );
        assert!(
            result.contains('\u{2026}'),
            "minimal path should contain ellipsis: {result}"
        );
    }

    #[test]
    fn test_ls_title_full() {
        let servers = vec!["rust".to_string(), "ts".to_string()];
        let result = degrade_ls_title(&servers, 40);
        assert!(
            result.contains("rust"),
            "full LS title should contain 'rust': {result}"
        );
        assert!(
            result.contains("ts"),
            "full LS title should contain 'ts': {result}"
        );
        assert!(
            result.contains('\u{25CF}'),
            "full LS title should contain status icon: {result}"
        );
    }

    #[test]
    fn test_ls_title_dropped() {
        let servers = vec!["rust".to_string(), "ts".to_string()];
        let result = degrade_ls_title(&servers, 5);
        assert!(
            result.is_empty(),
            "LS title should be empty at width 5: {result}"
        );
    }
}

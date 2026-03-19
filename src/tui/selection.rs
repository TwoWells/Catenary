// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Visual selection mode and clipboard yanking for event panels.
//!
//! Provides full-line visual selection (`v` mode), range highlighting, and
//! clipboard copy via platform clipboard commands (`xclip`, `xsel`,
//! `wl-copy`, `pbcopy`).

use std::io::Write as _;
use std::process::{Command, Stdio};

use anyhow::Result;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::flat::FlatLine;
use super::format::{
    format_ago, format_collapsed_plain, format_message_plain, format_pair_plain, format_scope_plain,
};
use super::panel::{PanelState, detail_lines, pair_detail_lines};
use super::tree::{SessionTree, TreeItem};

// ── Types ────────────────────────────────────────────────────────────────

/// Active visual selection within a panel.
#[derive(Debug, Clone)]
pub struct VisualSelection {
    /// Anchor line (where `v` was pressed).
    anchor: usize,
    /// Current end of selection (moves with j/k).
    cursor: usize,
}

/// Source of the selection (keyboard or mouse).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionSource {
    /// Started with `v` key, extends with j/k.
    Keyboard,
    /// Started with mouse click-drag.
    Mouse,
}

// ── VisualSelection ──────────────────────────────────────────────────────

impl VisualSelection {
    /// Create a new selection at the anchor point.
    ///
    /// Cursor starts at anchor (single line selected).
    #[must_use]
    pub const fn new(anchor: usize) -> Self {
        Self {
            anchor,
            cursor: anchor,
        }
    }

    /// Update the cursor end of the selection.
    ///
    /// Called on `j`/`k` in visual mode or during mouse drag.
    pub const fn extend(&mut self, new_cursor: usize) {
        self.cursor = new_cursor;
    }

    /// Return `(start, end)` inclusive, where `start <= end`.
    ///
    /// Handles both forward (anchor < cursor) and backward (anchor > cursor)
    /// selections.
    #[must_use]
    pub const fn range(&self) -> (usize, usize) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// Whether the given line index is within the selection range.
    #[must_use]
    pub const fn contains(&self, index: usize) -> bool {
        let (start, end) = self.range();
        index >= start && index <= end
    }

    /// Number of selected lines.
    #[must_use]
    pub const fn line_count(&self) -> usize {
        let (start, end) = self.range();
        end - start + 1
    }
}

// ── Yank ─────────────────────────────────────────────────────────────────

/// Format a single `FlatLine` as plain text for yank.
fn flat_line_plain(fl: &FlatLine, all_flat: &[FlatLine], panel: &PanelState<'_>) -> String {
    match fl {
        FlatLine::MessageHeader {
            message_index,
            paired_response,
        } => panel
            .messages
            .get(*message_index)
            .map_or_else(String::new, |msg| {
                paired_response
                    .and_then(|ri| panel.messages.get(ri))
                    .map_or_else(
                        || format_message_plain(msg),
                        |resp| format_pair_plain(msg, resp),
                    )
            }),
        FlatLine::Detail {
            message_index,
            detail_index,
        } => {
            let resp_idx = all_flat.iter().find_map(|fl2| {
                if let FlatLine::MessageHeader {
                    message_index: mi,
                    paired_response: Some(ri),
                } = fl2
                    && *mi == *message_index
                {
                    Some(*ri)
                } else {
                    None
                }
            });
            panel
                .messages
                .get(*message_index)
                .map_or_else(String::new, |msg| {
                    let details = resp_idx.and_then(|ri| panel.messages.get(ri)).map_or_else(
                        || detail_lines(msg, panel.theme),
                        |resp| pair_detail_lines(msg, resp, panel.theme),
                    );
                    details.get(*detail_index).map_or_else(String::new, |line| {
                        line.spans.iter().map(|s| s.content.as_ref()).collect()
                    })
                })
        }
        FlatLine::CollapsedHeader {
            start_index,
            end_index,
            count,
        } => format_collapsed_plain(&panel.messages, *start_index, *end_index, *count),
        FlatLine::ScopeHeader {
            parent,
            child_count,
            position,
            ..
        } => format_scope_plain(parent, *child_count, *position, &panel.messages),
        FlatLine::ScopeChild { depth, inner, .. } => {
            let indent = " ".repeat(depth * 4);
            let inner_text = flat_line_plain(inner, all_flat, panel);
            format!("{indent}{inner_text}")
        }
    }
}

/// Generate the clipboard text for the selection.
///
/// For each line in the selection range, formats the `FlatLine` as
/// plain text. Lines are separated by `\n`.
#[must_use]
pub fn yank_text(panel: &PanelState<'_>, selection: &VisualSelection) -> String {
    let flat = panel.flat_lines();
    let (start, end) = selection.range();
    let mut lines: Vec<String> = Vec::with_capacity(end.saturating_sub(start) + 1);

    for fl in flat.iter().skip(start).take(end - start + 1) {
        lines.push(flat_line_plain(fl, &flat, panel));
    }

    lines.join("\n")
}

/// Generate the clipboard text for a sessions tree selection.
///
/// For each visible item in the selection range:
/// - `TreeItem::Workspace`: the workspace path.
/// - `TreeItem::Session`: `"<id_short>  <client>  <status>  <age>"`.
///
/// Lines are separated by `\n`.
#[must_use]
pub fn yank_tree_text(tree: &SessionTree, selection: &VisualSelection) -> String {
    let items = tree.visible_items();
    let (start, end) = selection.range();
    let mut lines: Vec<String> = Vec::with_capacity(end.saturating_sub(start) + 1);

    for item in items.iter().skip(start).take(end - start + 1) {
        match item {
            TreeItem::Workspace { node, .. } => {
                lines.push(node.path.clone());
            }
            TreeItem::Session { row, .. } => {
                let id_short = if row.info.id.len() > 8 {
                    &row.info.id[..8]
                } else {
                    &row.info.id
                };
                let client = row.info.client_name.as_deref().unwrap_or("unknown");
                let status = if row.alive { "active" } else { "dead" };
                let age = format_ago(row.info.started_at);
                lines.push(format!("{id_short}  {client}  {status}  {age}"));
            }
        }
    }

    lines.join("\n")
}

// ── Rendering ────────────────────────────────────────────────────────────

/// Apply the highlight style to selected lines visible in the viewport.
///
/// For each row in `content_area`, computes the flat-line index as
/// `visible_start + row_offset`. If `selection.contains(index)`, overwrites
/// the row's style with the selection style.
///
/// Called after event lines are rendered, so it overlays the highlight on
/// top of existing styled content.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal coordinates are always small"
)]
pub fn render_selection_highlight(
    selection: &VisualSelection,
    visible_start: usize,
    buf: &mut Buffer,
    content_area: Rect,
    style: Style,
) {
    for row_offset in 0..content_area.height {
        let index = visible_start + row_offset as usize;
        if selection.contains(index) {
            let y = content_area.y + row_offset;
            for x in content_area.x..content_area.x + content_area.width {
                buf[(x, y)].set_style(style);
            }
        }
    }
}

// ── Clipboard ────────────────────────────────────────────────────────────

/// Copy text to the system clipboard.
///
/// Tries platform clipboard commands in order: `wl-copy` (Wayland),
/// `xclip` (X11), `xsel` (X11), `pbcopy` (macOS). If none are available,
/// silently succeeds (no error).
///
/// # Errors
///
/// Returns an error if a clipboard command is found but fails to execute.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    let commands: &[&[&str]] = &[
        &["wl-copy"],
        &["xclip", "-selection", "clipboard"],
        &["xsel", "--clipboard", "--input"],
        &["pbcopy"],
    ];

    for cmd in commands {
        let program = cmd[0];
        let args = &cmd[1..];

        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        match child {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(text.as_bytes())?;
                }
                child.wait()?;
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Command not found, try the next one.
            }
            Err(e) => {
                return Err(e.into());
            }
        }
    }

    // No clipboard command available — silently skip.
    Ok(())
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
    use ratatui::style::Modifier;

    use chrono::{TimeDelta, Utc};

    use crate::config::IconConfig;
    use crate::session::{SessionInfo, SessionMessage};
    use crate::tui::data::SessionRow;
    use crate::tui::icons::IconSet;
    use crate::tui::panel::PanelState;
    use crate::tui::theme::Theme;
    use crate::tui::tree::SessionTree;

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

    #[test]
    fn test_selection_new_single_line() {
        let sel = VisualSelection::new(5);
        assert_eq!(sel.range(), (5, 5));
        assert_eq!(sel.line_count(), 1);
        assert!(sel.contains(5));
        assert!(!sel.contains(4));
        assert!(!sel.contains(6));
    }

    #[test]
    fn test_selection_extend_forward() {
        let mut sel = VisualSelection::new(3);
        sel.extend(7);
        assert_eq!(sel.range(), (3, 7));
        assert_eq!(sel.line_count(), 5);
        assert!(sel.contains(5));
    }

    #[test]
    fn test_selection_extend_backward() {
        let mut sel = VisualSelection::new(7);
        sel.extend(3);
        assert_eq!(sel.range(), (3, 7));
        assert_eq!(sel.line_count(), 5);
        assert!(sel.contains(5));
    }

    #[test]
    fn test_selection_contains_boundaries() {
        let mut sel = VisualSelection::new(3);
        sel.extend(7);
        assert!(sel.contains(3), "start boundary should be inclusive");
        assert!(sel.contains(7), "end boundary should be inclusive");
        assert!(!sel.contains(2));
        assert!(!sel.contains(8));
    }

    #[test]
    fn test_yank_text_headers_only() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        // Use hook messages (never collapse) so each gets its own flat line.
        let messages: Vec<SessionMessage> = (0..5)
            .map(|i| make_message("hook", &format!("test-{i}"), "catenary"))
            .collect();
        panel.load_messages(messages);

        let sel = {
            let mut s = VisualSelection::new(1);
            s.extend(3);
            s
        };
        let text = yank_text(&panel, &sel);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(line.contains("test-"), "each line should contain method");
        }
    }

    #[test]
    fn test_yank_text_with_detail() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let messages = vec![
            make_message("lsp", "initialized", "rust-analyzer"),
            make_message_with_payload(
                "hook",
                "post-tool",
                "catenary",
                serde_json::json!({
                    "file": "/src/lib.rs",
                    "count": 2,
                    "preview": "\t:12:1 [error] rustc: bad thing\n\t:34:1 [warning] rustc: meh"
                }),
            ),
            make_message("mcp", "tools/list", "catenary"),
        ];
        panel.load_messages(messages);
        panel.expanded.insert(1);

        // Select header + first two detail lines of message 1.
        let sel = {
            let mut s = VisualSelection::new(1);
            s.extend(3);
            s
        };
        let text = yank_text(&panel, &sel);
        assert!(text.contains("lib.rs"), "header should mention file");
        // Detail lines contain pretty-printed payload.
        assert!(text.contains("post-tool"), "detail should contain method");
        assert!(
            text.lines().count() >= 3,
            "should have at least 3 lines of output"
        );
    }

    #[test]
    fn test_selection_survives_push_message() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let messages: Vec<SessionMessage> = (0..10)
            .map(|_| make_message("lsp", "initialized", "rust-analyzer"))
            .collect();
        panel.load_messages(messages);
        panel.tail_attached = false;

        let mut sel = VisualSelection::new(3);
        sel.extend(7);
        assert_eq!(sel.range(), (3, 7));

        // Push a new message (appends at end).
        panel.push_message(make_message("mcp", "tools/list", "catenary"));
        assert_eq!(panel.messages.len(), 11);

        // Selection range is unchanged — indices are stable when appending.
        assert_eq!(sel.range(), (3, 7));
        assert!(sel.contains(5));
    }

    #[test]
    fn test_render_selection_highlight() {
        let theme = test_theme();

        // Test render_selection_highlight in isolation (no render_panel).
        let backend = TestBackend::new(60, 7);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                // Fill buffer with plain text rows to simulate rendered content.
                for row in 0..area.height {
                    let y = area.y + row;
                    for x in area.x..area.x + area.width {
                        f.buffer_mut()[(x, y)].set_symbol("x");
                    }
                }

                // Selection covers flat-line indices 1-3, viewport starts at 0.
                let sel = {
                    let mut s = VisualSelection::new(1);
                    s.extend(3);
                    s
                };
                render_selection_highlight(&sel, 0, f.buffer_mut(), area, theme.selection);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let x = 0u16;

        // Rows 1-3 should have REVERSED modifier.
        for row in 1..=3u16 {
            let cell = &buf[(x, row)];
            assert!(
                cell.modifier.contains(Modifier::REVERSED),
                "row {row} should have REVERSED modifier"
            );
        }

        // Row 0 and row 4 should NOT have REVERSED.
        let cell_0 = &buf[(x, 0)];
        assert!(
            !cell_0.modifier.contains(Modifier::REVERSED),
            "row 0 should not have REVERSED"
        );
        let cell_4 = &buf[(x, 4)];
        assert!(
            !cell_4.modifier.contains(Modifier::REVERSED),
            "row 4 should not have REVERSED"
        );
    }

    #[test]
    fn test_copy_to_clipboard_helper() {
        // This test verifies that copy_to_clipboard doesn't panic or error
        // even when no clipboard command is available. It's a best-effort
        // test since CI environments may not have clipboard tools.
        let result = copy_to_clipboard("test selection text");
        assert!(
            result.is_ok(),
            "copy_to_clipboard should not error (even without clipboard tools)"
        );
    }

    fn make_session(id: &str, workspace: &str, alive: bool, mins_ago: i64) -> SessionRow {
        SessionRow {
            info: SessionInfo {
                id: id.to_string(),
                pid: 1234,
                workspace: workspace.to_string(),
                started_at: Utc::now() - TimeDelta::minutes(mins_ago),
                client_name: Some("test-client".to_string()),
                client_version: None,
                client_session_id: None,
            },
            alive,
            languages: vec![],
        }
    }

    #[test]
    fn test_yank_tree_text_sessions() {
        let sessions = vec![
            make_session("aaa11111", "/ws/alpha", true, 5),
            make_session("bbb22222", "/ws/alpha", false, 10),
        ];
        let tree = SessionTree::from_sessions(sessions);
        // Visible items: [Workspace(/ws/alpha), Session(aaa11111), Session(bbb22222)]
        // Select all three items (indices 0..=2).
        let sel = {
            let mut s = VisualSelection::new(0);
            s.extend(2);
            s
        };
        let text = yank_tree_text(&tree, &sel);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].contains("/ws/alpha"),
            "first line should be workspace path"
        );
        assert!(
            lines[1].contains("aaa11111"),
            "second line should contain session ID"
        );
        assert!(
            lines[1].contains("active"),
            "active session should say 'active'"
        );
        assert!(
            lines[2].contains("bbb22222"),
            "third line should contain session ID"
        );
        assert!(lines[2].contains("dead"), "dead session should say 'dead'");
        assert!(
            lines[1].contains("test-client"),
            "should include client name"
        );
    }

    #[test]
    fn test_yank_tree_text_workspace_only() {
        let sessions = vec![make_session("aaa11111", "/ws/alpha", true, 5)];
        let tree = SessionTree::from_sessions(sessions);
        // Select only the workspace row (index 0).
        let sel = VisualSelection::new(0);
        let text = yank_tree_text(&tree, &sel);
        assert_eq!(text, "/ws/alpha");
    }
}

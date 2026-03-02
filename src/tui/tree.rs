// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Sessions tree widget: hierarchical workspace → session view with
//! navigation, collapse/expand, and rendering.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use super::data::SessionRow;
use super::theme::{IconSet, Theme};

// ── Data types ──────────────────────────────────────────────────────────

/// A workspace node in the sessions tree.
pub struct WorkspaceNode {
    /// Workspace path (e.g., "~/Projects/Catenary").
    pub path: String,
    /// Sessions under this workspace, already sorted.
    pub sessions: Vec<SessionRow>,
    /// Whether the workspace node is collapsed (children hidden).
    pub collapsed: bool,
    /// True if any child session is alive.
    pub has_active: bool,
}

/// A single visible item in the flattened tree.
pub enum TreeItem<'a> {
    /// A workspace parent node.
    Workspace {
        /// Reference to the workspace node.
        node: &'a WorkspaceNode,
        /// Index into `SessionTree::workspaces`.
        index: usize,
    },
    /// A session child node.
    Session {
        /// Reference to the session row.
        row: &'a SessionRow,
        /// Index of the parent workspace in `SessionTree::workspaces`.
        workspace_index: usize,
    },
}

/// Identifies what the cursor is pointing at, without borrowing the tree.
enum CursorTarget {
    /// Cursor is on a workspace node at this index.
    Workspace(usize),
    /// Cursor is on a session under the given workspace index.
    Session {
        workspace_index: usize,
        session_id: String,
    },
}

/// State for the Sessions tree widget.
pub struct SessionTree {
    /// Workspace nodes, each containing sorted sessions.
    pub workspaces: Vec<WorkspaceNode>,
    /// Cursor position in the flat visible list.
    pub cursor: usize,
    /// Whether the cheatsheet is visible (toggled by `?`).
    pub show_cheatsheet: bool,
}

// ── Construction & navigation ───────────────────────────────────────────

impl SessionTree {
    /// Build a tree from a flat list of session rows.
    ///
    /// Groups sessions by workspace path. Within each workspace, sorts
    /// active sessions first, then inactive, each sub-group in reverse
    /// chronological order of `started_at`. Workspaces are sorted the same
    /// way (active-first based on whether any child is active, then by most
    /// recent session).
    #[must_use]
    pub fn from_sessions(sessions: Vec<SessionRow>) -> Self {
        use std::collections::BTreeMap;

        // Group by workspace path, preserving insertion order via BTreeMap.
        let mut groups: BTreeMap<String, Vec<SessionRow>> = BTreeMap::new();
        for row in sessions {
            groups
                .entry(row.info.workspace.clone())
                .or_default()
                .push(row);
        }

        let mut workspaces: Vec<WorkspaceNode> = groups
            .into_iter()
            .map(|(path, mut sessions)| {
                // Sort within workspace: active first, then reverse chronological.
                sessions.sort_by(|a, b| {
                    b.alive
                        .cmp(&a.alive)
                        .then_with(|| b.info.started_at.cmp(&a.info.started_at))
                });
                let has_active = sessions.iter().any(|s| s.alive);
                WorkspaceNode {
                    path,
                    sessions,
                    collapsed: false,
                    has_active,
                }
            })
            .collect();

        // Sort workspaces: active-first, then by most recent session.
        workspaces.sort_by(|a, b| {
            b.has_active.cmp(&a.has_active).then_with(|| {
                let a_latest = a.sessions.first().map(|s| s.info.started_at);
                let b_latest = b.sessions.first().map(|s| s.info.started_at);
                b_latest.cmp(&a_latest)
            })
        });

        Self {
            workspaces,
            cursor: 0,
            show_cheatsheet: false,
        }
    }

    /// Flatten the tree into a linear list respecting collapsed state.
    ///
    /// Each workspace always appears. Its sessions appear only if not
    /// collapsed.
    #[must_use]
    pub fn visible_items(&self) -> Vec<TreeItem<'_>> {
        let mut items = Vec::new();
        for (i, ws) in self.workspaces.iter().enumerate() {
            items.push(TreeItem::Workspace { node: ws, index: i });
            if !ws.collapsed {
                for row in &ws.sessions {
                    items.push(TreeItem::Session {
                        row,
                        workspace_index: i,
                    });
                }
            }
        }
        items
    }

    /// Move cursor by `delta` within visible items. Clamps to bounds.
    #[allow(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        reason = "terminal item counts never overflow isize"
    )]
    pub fn navigate(&mut self, delta: isize) {
        let count = self.visible_items().len();
        if count == 0 {
            self.cursor = 0;
            return;
        }
        let max = (count - 1) as isize;
        let new = (self.cursor as isize + delta).clamp(0, max);
        self.cursor = new as usize;
    }

    /// Identify the cursor target without borrowing `self` beyond the call.
    fn cursor_target(&self) -> Option<CursorTarget> {
        let mut flat_idx = 0usize;
        for (wi, ws) in self.workspaces.iter().enumerate() {
            if flat_idx == self.cursor {
                return Some(CursorTarget::Workspace(wi));
            }
            flat_idx += 1;
            if !ws.collapsed {
                for row in &ws.sessions {
                    if flat_idx == self.cursor {
                        return Some(CursorTarget::Session {
                            workspace_index: wi,
                            session_id: row.info.id.clone(),
                        });
                    }
                    flat_idx += 1;
                }
            }
        }
        None
    }

    /// Toggle at the current cursor position.
    ///
    /// If cursor is on a workspace: toggle collapsed.
    /// If cursor is on a session: return the session ID (caller opens/focuses
    /// the Events panel).
    pub fn toggle_at_cursor(&mut self) -> Option<String> {
        match self.cursor_target()? {
            CursorTarget::Workspace(wi) => {
                self.workspaces[wi].collapsed = !self.workspaces[wi].collapsed;
                None
            }
            CursorTarget::Session { session_id, .. } => Some(session_id),
        }
    }

    /// Collapse at cursor — `h` key.
    ///
    /// If cursor is on an expanded workspace, collapse it.
    /// If cursor is on a session, collapse the parent workspace and move
    /// cursor to it.
    pub fn collapse_at_cursor(&mut self) {
        let Some(target) = self.cursor_target() else {
            return;
        };
        let wi = match target {
            CursorTarget::Workspace(wi) => wi,
            CursorTarget::Session {
                workspace_index, ..
            } => workspace_index,
        };
        self.workspaces[wi].collapsed = true;
        // Move cursor to the parent workspace row.
        let mut flat_idx = 0usize;
        for (i, _) in self.workspaces.iter().enumerate() {
            if i == wi {
                self.cursor = flat_idx;
                return;
            }
            flat_idx += 1;
            // After collapsing, children of wi are hidden, but other
            // workspaces still contribute their visible children.
            if i != wi && !self.workspaces[i].collapsed {
                flat_idx += self.workspaces[i].sessions.len();
            }
        }
    }

    /// Expand at cursor — `l` key.
    ///
    /// If cursor is on a collapsed workspace, expand it.
    pub fn expand_at_cursor(&mut self) {
        if let Some(CursorTarget::Workspace(wi)) = self.cursor_target() {
            self.workspaces[wi].collapsed = false;
        }
    }

    /// If cursor is on a session, return its ID.
    #[must_use]
    pub fn selected_session_id(&self) -> Option<&str> {
        let items = self.visible_items();
        items.get(self.cursor).and_then(|item| match item {
            TreeItem::Session { row, .. } => Some(row.info.id.as_str()),
            TreeItem::Workspace { .. } => None,
        })
    }
}

// ── Rendering ───────────────────────────────────────────────────────────

/// Cheatsheet key bindings shown when `show_cheatsheet` is true.
const CHEATSHEET: &[(&str, &str)] = &[
    ("j/k", "navigate"),
    ("Enter", "select/expand"),
    ("Space", "pin/unpin panel"),
    ("Tab", "focus next panel"),
    ("w", "cycle layout"),
    ("z", "center cursor"),
    ("v", "visual select"),
    ("y", "yank (copy)"),
    ("f/F", "filter / global"),
    ("Del", "delete session"),
    ("?", "toggle this help"),
    ("x", "close panel"),
    ("Esc", "unpin all / cancel"),
];

/// Format workspace path: use basename if full path exceeds `max_width`.
fn format_workspace_path(path: &str, max_width: usize) -> &str {
    if path.len() <= max_width {
        return path;
    }
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

/// Format a session age from `started_at`.
fn format_age(started: chrono::DateTime<chrono::Utc>) -> String {
    super::theme::format_ago(started)
}

/// Render the sessions tree into the given buffer area.
///
/// # Layout
///
/// - Workspace rows: `●`/`○` status icon, path (shortened to basename if
///   the full path is too wide).
/// - Session rows: indented, ID (first 8 chars), client name, age. The
///   currently selected session (cursor) shows `▐` prefix.
/// - If focused, the border uses `theme.border_focused`; otherwise dim.
/// - If `show_cheatsheet`, render the cheatsheet block below the tree
///   items, separated by `─── Keys ───`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    reason = "terminal coordinates are always small; match arms for tree items"
)]
pub fn render_tree(
    tree: &SessionTree,
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    icons: &IconSet,
    focused: bool,
    has_grid: bool,
) {
    if area.width < 4 || area.height < 1 {
        return;
    }

    let border_style = if focused {
        theme.border_focused
    } else {
        theme.border_unfocused
    };

    let border_set = if focused {
        ratatui::symbols::border::THICK
    } else {
        ratatui::symbols::border::PLAIN
    };

    let title_style = if focused {
        theme.title
    } else {
        theme.border_unfocused
    };

    // Sessions has no left border per PLAN.md L44-45.
    // When there's no Events grid, Sessions needs its own right border.
    let borders = if has_grid {
        ratatui::widgets::Borders::TOP
    } else {
        ratatui::widgets::Borders::TOP | ratatui::widgets::Borders::RIGHT
    };
    let block = ratatui::widgets::Block::default()
        .borders(borders)
        .border_set(border_set)
        .border_style(border_style)
        .title(Span::styled(" Sessions ", title_style));
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.width < 2 || inner.height < 1 {
        return;
    }

    let items = tree.visible_items();
    let max_width = inner.width as usize;
    let mut y = inner.y;
    let y_max = inner.y + inner.height;

    // Render tree items.
    for (i, item) in items.iter().enumerate() {
        if y >= y_max {
            break;
        }
        let is_cursor = i == tree.cursor;
        let line = match item {
            TreeItem::Workspace { node, .. } => {
                let collapse_icon = if node.collapsed {
                    &icons.workspace_closed
                } else {
                    &icons.workspace_open
                };
                let status_icon = if node.has_active { "● " } else { "○ " };
                let icon_style = if node.has_active {
                    theme.session_active
                } else {
                    theme.session_dead
                };
                let path = format_workspace_path(&node.path, max_width.saturating_sub(4));
                Line::from(vec![
                    Span::styled(
                        collapse_icon,
                        if is_cursor {
                            theme.selection
                        } else {
                            theme.text
                        },
                    ),
                    Span::styled(status_icon, icon_style),
                    Span::styled(path.to_string(), theme.text),
                ])
            }
            TreeItem::Session { row, .. } => {
                let prefix = if i == tree.cursor { "  ▐ " } else { "    " };
                let id_short = if row.info.id.len() > 8 {
                    &row.info.id[..8]
                } else {
                    &row.info.id
                };
                let client = row.info.client_name.as_deref().unwrap_or("unknown");
                let age = format_age(row.info.started_at);
                let style = if row.alive {
                    theme.session_active
                } else {
                    theme.session_dead
                };
                let cursor_style = if i == tree.cursor {
                    theme.selection
                } else {
                    Style::default()
                };
                Line::from(vec![
                    Span::styled(prefix, cursor_style),
                    Span::styled(format!("{id_short}  "), style),
                    Span::styled(format!("{client}  "), theme.session_meta),
                    Span::styled(age, theme.timestamp),
                ])
            }
        };
        buf.set_line(inner.x, y, &line, inner.width);
        if is_cursor {
            for x in inner.x..inner.x + inner.width {
                buf[(x, y)].set_style(theme.selection);
            }
        }
        y += 1;
    }

    // Render cheatsheet if visible.
    if tree.show_cheatsheet {
        if y >= y_max {
            return;
        }

        // Separator line.
        let sep_width = max_width.min(24);
        let mut sep = String::from("─── Keys ");
        while sep.len() < sep_width {
            sep.push('─');
        }
        let sep_line = Line::from(Span::styled(sep, theme.muted));
        buf.set_line(inner.x, y, &sep_line, inner.width);
        y += 1;

        // Key bindings.
        for (key, desc) in CHEATSHEET {
            if y >= y_max {
                break;
            }
            let line = Line::from(vec![
                Span::styled(format!("{key:<10}"), theme.hint_key),
                Span::styled((*desc).to_string(), theme.hint_label),
            ]);
            buf.set_line(inner.x, y, &line, inner.width);
            y += 1;
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::session::SessionInfo;

    fn make_session(id: &str, workspace: &str, alive: bool, mins_ago: i64) -> SessionRow {
        SessionRow {
            info: SessionInfo {
                id: id.to_string(),
                pid: 1234,
                workspace: workspace.to_string(),
                started_at: Utc::now() - TimeDelta::minutes(mins_ago),
                client_name: Some("test-client".to_string()),
                client_version: None,
            },
            alive,
            languages: vec![],
        }
    }

    #[test]
    fn test_tree_groups_by_workspace() {
        let sessions = vec![
            make_session("aaa11111", "/ws/alpha", true, 5),
            make_session("bbb22222", "/ws/beta", true, 3),
            make_session("ccc33333", "/ws/alpha", false, 10),
        ];
        let tree = SessionTree::from_sessions(sessions);
        assert_eq!(tree.workspaces.len(), 2);
        // Alpha has 2 sessions, beta has 1.
        let alpha = tree
            .workspaces
            .iter()
            .find(|w| w.path == "/ws/alpha")
            .expect("alpha workspace");
        assert_eq!(alpha.sessions.len(), 2);
        let beta = tree
            .workspaces
            .iter()
            .find(|w| w.path == "/ws/beta")
            .expect("beta workspace");
        assert_eq!(beta.sessions.len(), 1);
    }

    #[test]
    fn test_tree_sorts_active_first() {
        // Active session started 5m ago, dead session started 1m ago.
        // Active should come first despite being older.
        let sessions = vec![
            make_session("dead0001", "/ws/test", false, 1),
            make_session("live0001", "/ws/test", true, 5),
        ];
        let tree = SessionTree::from_sessions(sessions);
        assert_eq!(tree.workspaces.len(), 1);
        let ws = &tree.workspaces[0];
        assert_eq!(ws.sessions[0].info.id, "live0001");
        assert_eq!(ws.sessions[1].info.id, "dead0001");
    }

    #[test]
    fn test_tree_workspace_active_icon() {
        let sessions = vec![
            make_session("live0001", "/ws/active", true, 5),
            make_session("dead0001", "/ws/active", false, 10),
            make_session("dead0002", "/ws/dead", false, 3),
            make_session("dead0003", "/ws/dead", false, 7),
        ];
        let tree = SessionTree::from_sessions(sessions);
        let active_ws = tree
            .workspaces
            .iter()
            .find(|w| w.path == "/ws/active")
            .expect("active workspace");
        assert!(active_ws.has_active);
        let dead_ws = tree
            .workspaces
            .iter()
            .find(|w| w.path == "/ws/dead")
            .expect("dead workspace");
        assert!(!dead_ws.has_active);
    }

    #[test]
    fn test_tree_visible_items_expanded() {
        let sessions = vec![
            make_session("aaa11111", "/ws/alpha", true, 5),
            make_session("bbb22222", "/ws/beta", true, 3),
            make_session("ccc33333", "/ws/alpha", false, 10),
        ];
        let tree = SessionTree::from_sessions(sessions);
        let items = tree.visible_items();
        // 2 workspaces + 3 sessions = 5 items.
        assert_eq!(items.len(), 5);
        // First item should be a workspace.
        assert!(matches!(items[0], TreeItem::Workspace { .. }));
    }

    #[test]
    fn test_tree_visible_items_collapsed() {
        let sessions = vec![
            make_session("aaa11111", "/ws/alpha", true, 5),
            make_session("bbb22222", "/ws/beta", true, 3),
            make_session("ccc33333", "/ws/alpha", false, 10),
        ];
        let mut tree = SessionTree::from_sessions(sessions);
        // Find the workspace with 2 sessions (alpha) and collapse it.
        let alpha_idx = tree
            .workspaces
            .iter()
            .position(|w| w.path == "/ws/alpha")
            .expect("alpha workspace");
        tree.workspaces[alpha_idx].collapsed = true;
        let items = tree.visible_items();
        // Alpha collapsed (2 sessions hidden) → 1 ws + 1 ws + 1 session = 3.
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn test_tree_cursor_navigation() {
        let sessions = vec![
            make_session("aaa11111", "/ws/alpha", true, 5),
            make_session("bbb22222", "/ws/beta", true, 3),
        ];
        let mut tree = SessionTree::from_sessions(sessions);
        assert_eq!(tree.cursor, 0);

        tree.navigate(1);
        assert_eq!(tree.cursor, 1);

        tree.navigate(1);
        assert_eq!(tree.cursor, 2);

        // Navigate -1 from 2 goes to 1.
        tree.navigate(-1);
        assert_eq!(tree.cursor, 1);

        // Navigate to 0 and then try -1 — should clamp at 0.
        tree.navigate(-1);
        assert_eq!(tree.cursor, 0);
        tree.navigate(-1);
        assert_eq!(tree.cursor, 0);
    }

    #[test]
    fn test_tree_toggle_workspace() {
        let sessions = vec![
            make_session("aaa11111", "/ws/alpha", true, 5),
            make_session("bbb22222", "/ws/alpha", true, 3),
        ];
        let mut tree = SessionTree::from_sessions(sessions);
        // Cursor starts at 0, which is the workspace.
        assert!(!tree.workspaces[0].collapsed);
        let result = tree.toggle_at_cursor();
        assert!(result.is_none());
        assert!(tree.workspaces[0].collapsed);
        // Toggle again to expand.
        let result = tree.toggle_at_cursor();
        assert!(result.is_none());
        assert!(!tree.workspaces[0].collapsed);
    }

    #[test]
    fn test_tree_toggle_session() {
        let sessions = vec![make_session("aaa11111", "/ws/alpha", true, 5)];
        let mut tree = SessionTree::from_sessions(sessions);
        // Navigate to the session (index 1).
        tree.navigate(1);
        let result = tree.toggle_at_cursor();
        assert_eq!(result, Some("aaa11111".to_string()));
    }

    #[test]
    fn test_tree_collapse_expand_keys() {
        let sessions = vec![
            make_session("aaa11111", "/ws/alpha", true, 5),
            make_session("bbb22222", "/ws/alpha", true, 3),
        ];
        let mut tree = SessionTree::from_sessions(sessions);
        // Cursor on workspace (index 0).
        assert!(!tree.workspaces[0].collapsed);
        tree.collapse_at_cursor();
        assert!(tree.workspaces[0].collapsed);
        tree.expand_at_cursor();
        assert!(!tree.workspaces[0].collapsed);
    }

    #[test]
    fn test_tree_render_basic() {
        let sessions = vec![
            make_session("aaa11111", "/ws/alpha", true, 5),
            make_session("bbb22222", "/ws/alpha", false, 10),
        ];
        let tree = SessionTree::from_sessions(sessions);
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());

        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_tree(&tree, area, f.buffer_mut(), &theme, &icons, true, false);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        // Should contain workspace path and session IDs.
        assert!(content.contains("alpha"), "expected workspace basename");
        assert!(content.contains("aaa11111"), "expected first session ID");
        assert!(content.contains("bbb22222"), "expected second session ID");
        // Should contain status icon for active workspace.
        assert!(content.contains('●'), "expected active icon");
    }

    #[test]
    fn test_tree_render_cheatsheet() {
        let sessions = vec![make_session("aaa11111", "/ws/alpha", true, 5)];
        let mut tree = SessionTree::from_sessions(sessions);
        tree.show_cheatsheet = true;
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());

        let backend = TestBackend::new(40, 25);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_tree(&tree, area, f.buffer_mut(), &theme, &icons, true, false);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(content.contains("Keys"), "expected cheatsheet separator");
        assert!(content.contains("navigate"), "expected cheatsheet content");
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
}

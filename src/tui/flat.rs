// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Flat line generation: converts pipeline [`DisplayEntry`] trees into a
//! linear sequence of [`FlatLine`]s suitable for viewport slicing and rendering.
//!
//! The main entry point is [`PanelState::flat_lines()`], which runs the full
//! display pipeline (pair merge → scope collapse → run collapse) and then
//! flattens the result, expanding scopes and detail lines as needed.

use std::rc::Rc;

use super::format::{
    format_collapsed_plain, format_message_plain, format_pair_plain, format_scope_plain,
};
use super::panel::{PanelState, detail_lines, pair_detail_lines};
use super::pipeline::{DisplayEntry, SegmentPosition, pair_merge, run_collapse, scope_collapse};

/// A line in the flattened view — message header, detail, or collapsed run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlatLine {
    /// A message header (the one-line summary).
    MessageHeader {
        /// Index into the messages vec (request index for pairs).
        message_index: usize,
        /// If this header is a merged pair, the response message index.
        paired_response: Option<usize>,
    },
    /// A detail line within an expanded message.
    Detail {
        /// Index into the messages vec (request index for pairs).
        message_index: usize,
        /// Index of this detail line within the expansion.
        detail_index: usize,
    },
    /// A collapsed run header (summary of N consecutive messages).
    CollapsedHeader {
        /// Index of the first message in the run.
        start_index: usize,
        /// Index of the last message in the run (inclusive).
        end_index: usize,
        /// Number of messages in the run.
        count: usize,
    },
    /// A scope header (parent of grouped children).
    ScopeHeader {
        /// The parent `DisplayEntry` (for rendering the summary line).
        parent: Rc<DisplayEntry>,
        /// Number of child entries in the scope.
        child_count: usize,
        /// Segment position within a segmented scope.
        position: SegmentPosition,
        /// Expansion key (first child's message index for this segment).
        expansion_key: usize,
    },
    /// An indented child line within an expanded scope.
    ScopeChild {
        /// Depth level for indentation.
        depth: usize,
        /// Message index of the scope parent (expansion key).
        scope_parent_index: usize,
        /// The inner `FlatLine` (`MessageHeader`, `CollapsedHeader`, etc.).
        inner: Box<Self>,
    },
}

impl PanelState<'_> {
    /// Build a flat list of lines: message headers interleaved with detail
    /// lines for any expanded messages. Uses pair merge then run collapse
    /// to combine adjacent request/response pairs and consecutive same-key
    /// messages into single lines.
    #[must_use]
    #[allow(
        clippy::too_many_lines,
        reason = "scope collapse adds a fourth variant arm"
    )]
    pub fn flat_lines(&self) -> Vec<FlatLine> {
        let lower_pattern = self.filter_pattern.as_ref().map(|p| p.to_lowercase());
        let merged = pair_merge(&self.messages);
        let scoped = scope_collapse(merged, &self.messages);
        let entries = run_collapse(scoped, &self.messages);
        let mut lines = Vec::new();

        for entry in &entries {
            match entry {
                DisplayEntry::Single {
                    index,
                    parent_id: _,
                } => {
                    let index = *index;
                    let msg = &self.messages[index];
                    if let Some(ref pat) = lower_pattern {
                        let plain = format_message_plain(msg);
                        if !plain.to_lowercase().contains(pat) {
                            continue;
                        }
                    }
                    lines.push(FlatLine::MessageHeader {
                        message_index: index,
                        paired_response: None,
                    });
                    if self.expanded.contains(&index) {
                        let count = detail_lines(msg, self.theme).len();
                        for detail_index in 0..count {
                            lines.push(FlatLine::Detail {
                                message_index: index,
                                detail_index,
                            });
                        }
                    }
                }
                DisplayEntry::Paired {
                    request_index,
                    response_index,
                    parent_id: _,
                } => {
                    let request_index = *request_index;
                    let response_index = *response_index;
                    let req = &self.messages[request_index];
                    let resp = &self.messages[response_index];
                    if let Some(ref pat) = lower_pattern {
                        let plain = format_pair_plain(req, resp);
                        if !plain.to_lowercase().contains(pat) {
                            continue;
                        }
                    }
                    lines.push(FlatLine::MessageHeader {
                        message_index: request_index,
                        paired_response: Some(response_index),
                    });
                    if self.expanded.contains(&request_index) {
                        let count = pair_detail_lines(req, resp, self.theme).len();
                        for detail_index in 0..count {
                            lines.push(FlatLine::Detail {
                                message_index: request_index,
                                detail_index,
                            });
                        }
                    }
                }
                DisplayEntry::Collapsed {
                    start_index,
                    end_index,
                    count,
                    parent_id: _,
                } => {
                    let start_index = *start_index;
                    let end_index = *end_index;
                    let count = *count;
                    if let Some(ref pat) = lower_pattern {
                        let plain =
                            format_collapsed_plain(&self.messages, start_index, end_index, count);
                        if !plain.to_lowercase().contains(pat) {
                            continue;
                        }
                    }
                    if self.expanded.contains(&start_index) {
                        // Show individual messages when expanded.
                        for idx in start_index..=end_index {
                            lines.push(FlatLine::MessageHeader {
                                message_index: idx,
                                paired_response: None,
                            });
                            if self.expanded.contains(&idx) {
                                let msg = &self.messages[idx];
                                let detail_count = detail_lines(msg, self.theme).len();
                                for detail_index in 0..detail_count {
                                    lines.push(FlatLine::Detail {
                                        message_index: idx,
                                        detail_index,
                                    });
                                }
                            }
                        }
                    } else {
                        lines.push(FlatLine::CollapsedHeader {
                            start_index,
                            end_index,
                            count,
                        });
                    }
                }
                DisplayEntry::Scope {
                    parent,
                    children,
                    position,
                } => {
                    let child_count = children.len();
                    if let Some(ref pat) = lower_pattern {
                        let plain =
                            format_scope_plain(parent, child_count, *position, &self.messages);
                        if !plain.to_lowercase().contains(pat) {
                            continue;
                        }
                    }
                    let scope_key = entry.expansion_index();
                    lines.push(FlatLine::ScopeHeader {
                        parent: Rc::clone(parent),
                        child_count,
                        position: *position,
                        expansion_key: scope_key,
                    });
                    if self.expanded.contains(&scope_key) {
                        self.flatten_scope_children(children, scope_key, 1, &mut lines);
                    }
                }
            }
        }
        lines
    }

    /// Flatten scope children into `ScopeChild` flat lines.
    fn flatten_scope_children(
        &self,
        children: &[DisplayEntry],
        scope_parent_index: usize,
        depth: usize,
        lines: &mut Vec<FlatLine>,
    ) {
        for child in children {
            match child {
                DisplayEntry::Single { index, .. } => {
                    let index = *index;
                    lines.push(FlatLine::ScopeChild {
                        depth,
                        scope_parent_index,
                        inner: Box::new(FlatLine::MessageHeader {
                            message_index: index,
                            paired_response: None,
                        }),
                    });
                    if self.expanded.contains(&index) {
                        let msg = &self.messages[index];
                        let count = detail_lines(msg, self.theme).len();
                        for detail_index in 0..count {
                            lines.push(FlatLine::ScopeChild {
                                depth,
                                scope_parent_index,
                                inner: Box::new(FlatLine::Detail {
                                    message_index: index,
                                    detail_index,
                                }),
                            });
                        }
                    }
                }
                DisplayEntry::Paired {
                    request_index,
                    response_index,
                    ..
                } => {
                    let request_index = *request_index;
                    let response_index = *response_index;
                    lines.push(FlatLine::ScopeChild {
                        depth,
                        scope_parent_index,
                        inner: Box::new(FlatLine::MessageHeader {
                            message_index: request_index,
                            paired_response: Some(response_index),
                        }),
                    });
                    if self.expanded.contains(&request_index) {
                        let req = &self.messages[request_index];
                        let resp = &self.messages[response_index];
                        let count = pair_detail_lines(req, resp, self.theme).len();
                        for detail_index in 0..count {
                            lines.push(FlatLine::ScopeChild {
                                depth,
                                scope_parent_index,
                                inner: Box::new(FlatLine::Detail {
                                    message_index: request_index,
                                    detail_index,
                                }),
                            });
                        }
                    }
                }
                DisplayEntry::Collapsed {
                    start_index,
                    end_index,
                    count,
                    ..
                } => {
                    lines.push(FlatLine::ScopeChild {
                        depth,
                        scope_parent_index,
                        inner: Box::new(FlatLine::CollapsedHeader {
                            start_index: *start_index,
                            end_index: *end_index,
                            count: *count,
                        }),
                    });
                }
                DisplayEntry::Scope {
                    parent: nested_parent,
                    children: nested_children,
                    position: nested_position,
                } => {
                    let nested_child_count = nested_children.len();
                    let nested_key = child.expansion_index();
                    lines.push(FlatLine::ScopeChild {
                        depth,
                        scope_parent_index,
                        inner: Box::new(FlatLine::ScopeHeader {
                            parent: Rc::clone(nested_parent),
                            child_count: nested_child_count,
                            position: *nested_position,
                            expansion_key: nested_key,
                        }),
                    });
                    if self.expanded.contains(&nested_key) {
                        self.flatten_scope_children(nested_children, nested_key, depth + 1, lines);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect/panic for readable assertions"
)]
mod tests {
    use super::*;
    use crate::config::IconConfig;
    use crate::session::SessionMessage;
    use crate::tui::icons::IconSet;
    use crate::tui::panel::{PanelState, detail_lines};
    use crate::tui::theme::Theme;

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

    fn make_non_collapsing_messages(n: usize) -> Vec<SessionMessage> {
        (0..n)
            .map(|i| make_message("hook", &format!("test-{i}"), "catenary"))
            .collect()
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

    fn make_hook_diag_message(file: &str, count: u64) -> SessionMessage {
        make_message_with_payload(
            "hook",
            "post-tool",
            "catenary",
            serde_json::json!({
                "file": file,
                "count": count,
                "preview": "\t:12:1 [error] rustc: something"
            }),
        )
    }

    fn make_message_with_id_parent(
        id: i64,
        r#type: &str,
        method: &str,
        server: &str,
        request_id: Option<i64>,
        parent_id: Option<i64>,
    ) -> SessionMessage {
        SessionMessage {
            id,
            r#type: r#type.to_string(),
            method: method.to_string(),
            server: server.to_string(),
            client: "catenary".to_string(),
            request_id,
            parent_id,
            timestamp: chrono::Utc::now(),
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn test_flat_lines_no_expansion() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(5);
        panel.load_messages(messages);

        let flat = panel.flat_lines();
        assert_eq!(flat.len(), 5);
        for (i, fl) in flat.iter().enumerate() {
            assert_eq!(
                *fl,
                FlatLine::MessageHeader {
                    message_index: i,
                    paired_response: None,
                }
            );
        }
    }

    #[test]
    fn test_flat_lines_one_expanded() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let messages = vec![
            make_message("lsp", "initialized", "rust-analyzer"),
            make_hook_diag_message("/src/lib.rs", 3),
            make_message("mcp", "tools/list", "catenary"),
        ];
        panel.load_messages(messages);
        panel.expanded.insert(1);

        let flat = panel.flat_lines();
        let detail_count = detail_lines(&panel.messages[1], &theme).len();
        assert!(detail_count > 0, "hook diag message should have details");
        assert_eq!(flat.len(), 3 + detail_count);
        assert_eq!(
            flat[0],
            FlatLine::MessageHeader {
                message_index: 0,
                paired_response: None,
            }
        );
        assert_eq!(
            flat[1],
            FlatLine::MessageHeader {
                message_index: 1,
                paired_response: None,
            }
        );
        for i in 0..detail_count {
            assert_eq!(
                flat[2 + i],
                FlatLine::Detail {
                    message_index: 1,
                    detail_index: i
                }
            );
        }
        assert_eq!(
            flat[2 + detail_count],
            FlatLine::MessageHeader {
                message_index: 2,
                paired_response: None,
            }
        );
    }

    #[test]
    fn test_scope_flat_lines_collapsed() {
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            make_message_with_id_parent(1, "mcp", "tools/call", "catenary", None, None),
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(1),
            ),
            make_message_with_id_parent(3, "lsp", "workspace/symbol", "taplo", None, Some(1)),
        ];
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(messages);

        let flat = panel.flat_lines();
        assert_eq!(flat.len(), 1, "collapsed scope should be 1 line: {flat:?}");
        assert!(
            matches!(flat[0], FlatLine::ScopeHeader { child_count: 2, .. }),
            "should be ScopeHeader with 2 children: {:?}",
            flat[0]
        );
    }

    #[test]
    fn test_scope_flat_lines_expanded() {
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            make_message_with_id_parent(1, "mcp", "tools/call", "catenary", None, None),
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(1),
            ),
            make_message_with_id_parent(3, "lsp", "workspace/symbol", "taplo", None, Some(1)),
        ];
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(messages);
        panel.expanded.insert(1);

        let flat = panel.flat_lines();
        assert_eq!(flat.len(), 3, "expanded scope should be 3 lines: {flat:?}");
        assert!(
            matches!(flat[0], FlatLine::ScopeHeader { .. }),
            "first should be ScopeHeader"
        );
        assert!(
            matches!(
                flat[1],
                FlatLine::ScopeChild {
                    depth: 1,
                    scope_parent_index: 1,
                    ..
                }
            ),
            "second should be ScopeChild at depth 1: {:?}",
            flat[1]
        );
        assert!(
            matches!(
                flat[2],
                FlatLine::ScopeChild {
                    depth: 1,
                    scope_parent_index: 1,
                    ..
                }
            ),
            "third should be ScopeChild at depth 1: {:?}",
            flat[2]
        );
    }
}

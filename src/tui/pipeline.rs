// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Display pipeline: data types and transformation passes.
//!
//! The pipeline converts raw [`SessionMessage`] sequences into display-ready
//! [`DisplayEntry`] trees through three passes:
//!
//! 1. **Pair merge** — adjacent request/response messages merge into `Paired`.
//! 2. **Scope collapse** — entries sharing a `parent_id` group into `Scope`.
//! 3. **Run collapse** — consecutive same-category singles merge into `Collapsed`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use super::category;
use crate::session::SessionMessage;

// ── Data types ──────────────────────────────────────────────────────────

/// Position of a scope segment in a segmented scope.
///
/// When a scope's children are interrupted by unrelated root-level events
/// (e.g., progress tokens), the scope is split into segments. Each segment
/// covers a contiguous run of children between interruptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentPosition {
    /// Single segment (no interruptions) — full scope.
    Only,
    /// First segment — scope opened, ongoing.
    First,
    /// Middle segment — continuation, still ongoing.
    Middle,
    /// Last segment — final, carries metrics.
    Last,
}

/// A display pipeline entry — single message, merged pair, collapsed run, or scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayEntry {
    /// A single message (not merged).
    Single {
        /// Index into the messages vec.
        index: usize,
        /// Scope/causation parent from the source message.
        parent_id: Option<i64>,
    },
    /// A request/response pair merged into one line.
    Paired {
        /// Index of the request message.
        request_index: usize,
        /// Index of the response message.
        response_index: usize,
        /// Scope/causation parent from the request message.
        parent_id: Option<i64>,
    },
    /// A run of consecutive messages collapsed into one line.
    Collapsed {
        /// Index of the first message in the run.
        start_index: usize,
        /// Index of the last message in the run (inclusive).
        end_index: usize,
        /// Number of messages in the run.
        count: usize,
        /// Scope/causation parent from the first message in the run.
        parent_id: Option<i64>,
    },
    /// A scope: a parent entry with child entries grouped under it.
    ///
    /// When scope children are interrupted by root-level events, the scope
    /// is split into segments. Each segment shares an `Rc` to the same
    /// parent entry; `position` indicates where in the sequence it falls.
    Scope {
        /// The parent entry (MCP tools/call request, typically Paired).
        /// Shared via `Rc` across segments of the same scope.
        parent: Rc<Self>,
        /// Child entries belonging to this segment.
        children: Vec<Self>,
        /// Position of this segment within the segmented scope.
        position: SegmentPosition,
    },
}

impl DisplayEntry {
    /// Extract the `parent_id` from any variant.
    #[must_use]
    pub fn parent_id(&self) -> Option<i64> {
        match self {
            Self::Single { parent_id, .. }
            | Self::Paired { parent_id, .. }
            | Self::Collapsed { parent_id, .. } => *parent_id,
            Self::Scope { parent, .. } => parent.parent_id(),
        }
    }

    /// Return the database message `id` for this entry's primary message.
    ///
    /// - Single: `messages[index].id`
    /// - Paired: `messages[request_index].id`
    /// - Collapsed: `messages[start_index].id`
    /// - Scope: delegates to parent
    #[must_use]
    pub fn message_id(&self, messages: &[SessionMessage]) -> i64 {
        match self {
            Self::Single { index, .. } => messages[*index].id,
            Self::Paired { request_index, .. } => messages[*request_index].id,
            Self::Collapsed { start_index, .. } => messages[*start_index].id,
            Self::Scope { parent, .. } => parent.message_id(messages),
        }
    }

    /// Return the primary message index (into the messages vec) for expansion key.
    ///
    /// - Single: `index`
    /// - Paired: `request_index`
    /// - Collapsed: `start_index`
    /// - Scope: first child's message index (unique per segment)
    #[must_use]
    pub fn expansion_index(&self) -> usize {
        match self {
            Self::Single { index, .. } => *index,
            Self::Paired { request_index, .. } => *request_index,
            Self::Collapsed { start_index, .. } => *start_index,
            Self::Scope {
                children,
                parent,
                position,
            } => match position {
                SegmentPosition::First | SegmentPosition::Only => parent.expansion_index(),
                _ => children
                    .first()
                    .map_or_else(|| parent.expansion_index(), Self::expansion_index),
            },
        }
    }
}

// ── Pipeline passes ─────────────────────────────────────────────────────

/// Run pair merge on a message list.
///
/// If a message has `request_id` set and the value equals the `id` of
/// the immediately preceding message, both merge into a single `Paired`
/// entry. Non-adjacent pairs remain as separate `Single` entries.
#[must_use]
pub fn pair_merge(messages: &[SessionMessage]) -> Vec<DisplayEntry> {
    let mut entries = Vec::with_capacity(messages.len());
    let mut i = 0;
    while i < messages.len() {
        if i + 1 < messages.len() && messages[i + 1].request_id == Some(messages[i].id) {
            entries.push(DisplayEntry::Paired {
                request_index: i,
                response_index: i + 1,
                parent_id: messages[i].parent_id,
            });
            i += 2;
        } else {
            entries.push(DisplayEntry::Single {
                index: i,
                parent_id: messages[i].parent_id,
            });
            i += 1;
        }
    }
    entries
}

/// Per-scope state used during the segmentation scan in [`scope_collapse`].
struct ScopeBuilder {
    /// Shared parent entry (one allocation, all segments reference it).
    parent: Rc<DisplayEntry>,
    /// Original entry index of the parent (for ordering in final assembly).
    parent_idx: usize,
    /// Children accumulated in the current (open) segment.
    current_segment: Vec<DisplayEntry>,
    /// Original entry index of the current segment's first child.
    current_segment_start: usize,
    /// Completed segments: (first child's entry index, children).
    completed_segments: Vec<(usize, Vec<DisplayEntry>)>,
}

impl ScopeBuilder {
    /// Close the current segment if it has any children.
    fn close_segment(&mut self) {
        if !self.current_segment.is_empty() {
            self.completed_segments.push((
                self.current_segment_start,
                std::mem::take(&mut self.current_segment),
            ));
        }
    }

    /// Record a child entry, tracking the start index for new segments.
    fn push_child(&mut self, entry_idx: usize, entry: DisplayEntry) {
        if self.current_segment.is_empty() {
            self.current_segment_start = entry_idx;
        }
        self.current_segment.push(entry);
    }

    /// Consume the builder into `(sort_key, Scope)` pairs.
    ///
    /// The first segment uses the parent's entry index as its sort key
    /// (so it appears at the parent's chronological position). Subsequent
    /// segments use their first child's entry index.
    fn into_keyed_scopes(mut self) -> Vec<(usize, DisplayEntry)> {
        self.close_segment();
        let total = self.completed_segments.len();
        self.completed_segments
            .into_iter()
            .enumerate()
            .map(|(i, (child_start, children))| {
                let position = match (i, total) {
                    (_, 1) => SegmentPosition::Only,
                    (0, _) => SegmentPosition::First,
                    (n, t) if n == t - 1 => SegmentPosition::Last,
                    _ => SegmentPosition::Middle,
                };
                let sort_key = if i == 0 { self.parent_idx } else { child_start };
                let scope = DisplayEntry::Scope {
                    parent: Rc::clone(&self.parent),
                    children,
                    position,
                };
                (sort_key, scope)
            })
            .collect()
    }
}

/// Scope collapse pass: group entries by `parent_id` into segmented `Scope` entries.
///
/// Entries whose `parent_id` points to another entry's message ID are
/// collected as children of that parent. When root-level entries (no
/// `parent_id` or orphaned) appear between children of an open scope,
/// the scope is split into segments separated by the interrupting events.
///
/// Each segment is an independent `Scope` entry sharing an `Rc` to the
/// same parent. Segment position (`Only`, `First`, `Middle`, `Last`)
/// drives the ellipsis rendering convention.
#[must_use]
pub fn scope_collapse(
    entries: Vec<DisplayEntry>,
    messages: &[SessionMessage],
) -> Vec<DisplayEntry> {
    if entries.is_empty() {
        return entries;
    }

    // First pass: collect the set of parent_id values referenced by any entry,
    // and map each entry's message ID to its index.
    let mut referenced_parents: HashSet<i64> = HashSet::new();
    let mut msg_id_to_idx: HashMap<i64, usize> = HashMap::new();

    for (i, entry) in entries.iter().enumerate() {
        let mid = entry.message_id(messages);
        msg_id_to_idx.insert(mid, i);
        if let Some(pid) = entry.parent_id() {
            referenced_parents.insert(pid);
        }
    }

    // Identify which entry indices are scope parents (their message ID is
    // referenced as a parent_id by another entry in the list).
    let scope_parent_indices: HashSet<usize> = referenced_parents
        .iter()
        .filter_map(|pid| msg_id_to_idx.get(pid).copied())
        .collect();

    // Second pass: stateful left-to-right scan. Track per-scope builders,
    // and output slots for root-level entries. Children are consumed by
    // their scope's builder. Root-level entries interrupt all open scopes.
    let len = entries.len();
    let mut builders: BTreeMap<usize, ScopeBuilder> = BTreeMap::new();
    // Output slots: (original_index, entry_or_placeholder).
    // Root-level entries go directly into slots. Scope parents reserve
    // a slot that will be expanded into segments in final assembly.
    let mut root_slots: Vec<(usize, DisplayEntry)> = Vec::new();

    for (i, entry) in entries.into_iter().enumerate() {
        if scope_parent_indices.contains(&i) {
            // Scope parent — create builder, no output slot yet.
            builders.insert(
                i,
                ScopeBuilder {
                    parent: Rc::new(entry),
                    parent_idx: i,
                    current_segment: Vec::new(),
                    current_segment_start: 0,
                    completed_segments: Vec::new(),
                },
            );
        } else if let Some(pid) = entry.parent_id() {
            if let Some(&parent_idx) = msg_id_to_idx.get(&pid)
                && let Some(builder) = builders.get_mut(&parent_idx)
            {
                // Child of a known scope — add to current segment.
                builder.push_child(i, entry);
                continue;
            }
            // Orphaned child — treat as root-level.
            for builder in builders.values_mut() {
                builder.close_segment();
            }
            root_slots.push((i, entry));
        } else {
            // Root-level entry — interrupts all open scopes.
            for builder in builders.values_mut() {
                builder.close_segment();
            }
            root_slots.push((i, entry));
        }
    }

    // Nested scope resolution: inner builders whose parent has a parent_id
    // referencing another builder become children of that outer builder.
    // Loop until stable so deeper nesting (A → B → C) resolves leaves first.
    loop {
        let inner_keys: Vec<usize> = builders
            .keys()
            .filter(|&&k| {
                let pid = builders[&k].parent.parent_id();
                pid.is_some_and(|p| {
                    msg_id_to_idx
                        .get(&p)
                        .is_some_and(|&idx| idx != k && builders.contains_key(&idx))
                })
            })
            .copied()
            .collect();

        if inner_keys.is_empty() {
            break;
        }

        for key in inner_keys {
            if let Some(inner_builder) = builders.remove(&key)
                && let Some(outer_parent_id) = inner_builder.parent.parent_id()
                && let Some(&outer_key) = msg_id_to_idx.get(&outer_parent_id)
                && let Some(outer_builder) = builders.get_mut(&outer_key)
            {
                let scopes = inner_builder.into_keyed_scopes();
                for (sort_key, scope_entry) in scopes {
                    outer_builder.push_child(sort_key, scope_entry);
                }
            }
        }
    }

    // Final assembly: merge scope segments and root-level entries in
    // original chronological order. Each segment gets its own sort key:
    // first segment at parent position, subsequent segments at their
    // first child's position.
    let mut ordered: Vec<(usize, DisplayEntry)> = Vec::with_capacity(len);

    for (idx, entry) in root_slots {
        ordered.push((idx, entry));
    }
    for (_, builder) in builders {
        ordered.extend(builder.into_keyed_scopes());
    }
    ordered.sort_by_key(|(idx, _)| *idx);

    ordered.into_iter().map(|(_, entry)| entry).collect()
}

/// Run collapse pass: merge consecutive `Single` entries with the same
/// collapse key into `Collapsed` entries.
///
/// `Paired` and `Scope` entries never collapse — they break the current
/// run and pass through as-is. Takes ownership to avoid cloning scope trees.
#[must_use]
pub fn run_collapse(entries: Vec<DisplayEntry>, messages: &[SessionMessage]) -> Vec<DisplayEntry> {
    let mut result = Vec::with_capacity(entries.len());

    // Current run state.
    let mut run_key: Option<String> = None;
    let mut run_start: usize = 0;
    let mut run_end: usize = 0;
    let mut run_count: usize = 0;

    let flush = |result: &mut Vec<DisplayEntry>,
                 key: &Option<String>,
                 start: usize,
                 end: usize,
                 count: usize,
                 msgs: &[SessionMessage]| {
        if key.is_none() || count == 0 {
            return;
        }
        let parent_id = msgs[start].parent_id;
        if count == 1 {
            result.push(DisplayEntry::Single {
                index: start,
                parent_id,
            });
        } else {
            result.push(DisplayEntry::Collapsed {
                start_index: start,
                end_index: end,
                count,
                parent_id,
            });
        }
    };

    for entry in entries {
        match entry {
            DisplayEntry::Single {
                index,
                parent_id: _,
            } => {
                let key = category::collapse_key(&messages[index]);
                if let Some(ref k) = key
                    && let Some(ref rk) = run_key
                    && k == rk
                {
                    // Extend current run.
                    run_end = index;
                    run_count += 1;
                } else {
                    // Flush previous run, start new one.
                    flush(
                        &mut result,
                        &run_key,
                        run_start,
                        run_end,
                        run_count,
                        messages,
                    );
                    if key.is_some() {
                        run_key = key;
                        run_start = index;
                        run_end = index;
                        run_count = 1;
                    } else {
                        run_key = None;
                        run_count = 0;
                        result.push(DisplayEntry::Single {
                            index,
                            parent_id: messages[index].parent_id,
                        });
                    }
                }
            }
            DisplayEntry::Paired { .. }
            | DisplayEntry::Collapsed { .. }
            | DisplayEntry::Scope { .. } => {
                // Flush any pending run, then emit as-is.
                flush(
                    &mut result,
                    &run_key,
                    run_start,
                    run_end,
                    run_count,
                    messages,
                );
                run_key = None;
                run_count = 0;
                result.push(entry);
            }
        }
    }

    // Flush trailing run.
    flush(
        &mut result,
        &run_key,
        run_start,
        run_end,
        run_count,
        messages,
    );

    result
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect/panic for readable assertions"
)]
mod tests {
    use std::rc::Rc;

    use super::*;
    use crate::session::SessionMessage;

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

    fn make_message_with_id(
        id: i64,
        r#type: &str,
        method: &str,
        server: &str,
        request_id: Option<i64>,
    ) -> SessionMessage {
        SessionMessage {
            id,
            r#type: r#type.to_string(),
            method: method.to_string(),
            server: server.to_string(),
            client: "catenary".to_string(),
            request_id,
            parent_id: None,
            timestamp: chrono::Utc::now(),
            payload: serde_json::json!({}),
        }
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

    fn make_progress_message(server: &str, token: &str) -> SessionMessage {
        make_message_with_payload(
            "lsp",
            "$/progress",
            server,
            serde_json::json!({"token": token}),
        )
    }

    // ── Pair merge tests ───────────────────────────────────────────────

    #[test]
    fn test_pair_merge_adjacent() {
        let messages = vec![
            make_message_with_id(1, "lsp", "textDocument/hover", "rust-analyzer", None),
            make_message_with_id(2, "lsp", "textDocument/hover", "rust-analyzer", Some(1)),
        ];
        let entries = pair_merge(&messages);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0],
            DisplayEntry::Paired {
                request_index: 0,
                response_index: 1,
                parent_id: None,
            }
        );
    }

    #[test]
    fn test_pair_merge_non_adjacent() {
        let messages = vec![
            make_message_with_id(1, "lsp", "textDocument/hover", "rust-analyzer", None),
            make_message_with_id(2, "lsp", "$/progress", "rust-analyzer", None),
            make_message_with_id(3, "lsp", "textDocument/hover", "rust-analyzer", Some(1)),
        ];
        let entries = pair_merge(&messages);
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[0],
            DisplayEntry::Single {
                index: 0,
                parent_id: None
            }
        );
        assert_eq!(
            entries[1],
            DisplayEntry::Single {
                index: 1,
                parent_id: None
            }
        );
        assert_eq!(
            entries[2],
            DisplayEntry::Single {
                index: 2,
                parent_id: None
            }
        );
    }

    #[test]
    fn test_pair_merge_consecutive_pairs() {
        let messages = vec![
            make_message_with_id(1, "lsp", "textDocument/hover", "rust-analyzer", None),
            make_message_with_id(2, "lsp", "textDocument/hover", "rust-analyzer", Some(1)),
            make_message_with_id(3, "lsp", "textDocument/definition", "rust-analyzer", None),
            make_message_with_id(
                4,
                "lsp",
                "textDocument/definition",
                "rust-analyzer",
                Some(3),
            ),
        ];
        let entries = pair_merge(&messages);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            DisplayEntry::Paired {
                request_index: 0,
                response_index: 1,
                parent_id: None,
            }
        );
        assert_eq!(
            entries[1],
            DisplayEntry::Paired {
                request_index: 2,
                response_index: 3,
                parent_id: None,
            }
        );
    }

    #[test]
    fn test_pair_merge_no_request_id() {
        let messages = vec![
            make_message_with_id(1, "lsp", "$/progress", "rust-analyzer", None),
            make_message_with_id(2, "lsp", "$/progress", "rust-analyzer", None),
            make_message_with_id(3, "lsp", "$/progress", "rust-analyzer", None),
        ];
        let entries = pair_merge(&messages);
        assert_eq!(entries.len(), 3);
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(
                *entry,
                DisplayEntry::Single {
                    index: i,
                    parent_id: None
                }
            );
        }
    }

    // ── Run collapse tests ───────────────────────────────────────────────

    #[test]
    fn test_run_collapse_consecutive_progress() {
        let messages = vec![
            make_progress_message("rust-analyzer", "ra/indexing"),
            make_progress_message("rust-analyzer", "ra/indexing"),
            make_progress_message("rust-analyzer", "ra/indexing"),
        ];
        let entries = pair_merge(&messages);
        let collapsed = run_collapse(entries, &messages);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(
            collapsed[0],
            DisplayEntry::Collapsed {
                start_index: 0,
                end_index: 2,
                count: 3,
                parent_id: None,
            }
        );
    }

    #[test]
    fn test_run_collapse_split_by_different_key() {
        let messages = vec![
            make_progress_message("rust-analyzer", "ra/indexing"),
            make_progress_message("rust-analyzer", "ra/flycheck"),
        ];
        let entries = pair_merge(&messages);
        let collapsed = run_collapse(entries, &messages);
        assert_eq!(collapsed.len(), 2);
        assert_eq!(
            collapsed[0],
            DisplayEntry::Single {
                index: 0,
                parent_id: None
            }
        );
        assert_eq!(
            collapsed[1],
            DisplayEntry::Single {
                index: 1,
                parent_id: None
            }
        );
    }

    #[test]
    fn test_run_collapse_split_by_interleaving() {
        let messages = vec![
            make_progress_message("rust-analyzer", "ra/indexing"),
            make_message_with_payload(
                "mcp",
                "tools/call",
                "catenary",
                serde_json::json!({"params": {"name": "grep"}}),
            ),
            make_progress_message("rust-analyzer", "ra/indexing"),
        ];
        let entries = pair_merge(&messages);
        let collapsed = run_collapse(entries, &messages);
        assert_eq!(
            collapsed.len(),
            3,
            "interleaving tool call should split the run"
        );
    }

    #[test]
    fn test_run_collapse_paired_not_collapsed() {
        let messages = vec![
            make_message_with_id(1, "lsp", "textDocument/hover", "rust-analyzer", None),
            make_message_with_id(2, "lsp", "textDocument/hover", "rust-analyzer", Some(1)),
            make_message_with_id(3, "lsp", "textDocument/hover", "rust-analyzer", None),
            make_message_with_id(4, "lsp", "textDocument/hover", "rust-analyzer", Some(3)),
        ];
        let entries = pair_merge(&messages);
        assert_eq!(entries.len(), 2, "should have 2 pairs");
        let collapsed = run_collapse(entries, &messages);
        assert_eq!(collapsed.len(), 2, "pairs should not collapse");
        assert!(
            matches!(collapsed[0], DisplayEntry::Paired { .. }),
            "first should be Paired"
        );
        assert!(
            matches!(collapsed[1], DisplayEntry::Paired { .. }),
            "second should be Paired"
        );
    }

    #[test]
    fn test_run_collapse_single_message_no_collapse() {
        let messages = vec![make_progress_message("rust-analyzer", "ra/indexing")];
        let entries = pair_merge(&messages);
        let collapsed = run_collapse(entries, &messages);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(
            collapsed[0],
            DisplayEntry::Single {
                index: 0,
                parent_id: None
            },
            "single message should not collapse"
        );
    }

    // ── parent_id propagation tests ─────────────────────────────────────

    #[test]
    fn test_pair_merge_propagates_parent_id() {
        let messages = vec![
            make_message_with_id_parent(
                1,
                "lsp",
                "textDocument/hover",
                "rust-analyzer",
                None,
                Some(100),
            ),
            make_message_with_id_parent(
                2,
                "lsp",
                "textDocument/hover",
                "rust-analyzer",
                Some(1),
                Some(100),
            ),
        ];
        let entries = pair_merge(&messages);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0],
            DisplayEntry::Paired {
                request_index: 0,
                response_index: 1,
                parent_id: Some(100),
            }
        );
    }

    #[test]
    fn test_pair_merge_none_parent_id() {
        let messages = vec![
            make_message_with_id_parent(
                1,
                "lsp",
                "textDocument/hover",
                "rust-analyzer",
                None,
                None,
            ),
            make_message_with_id_parent(
                2,
                "lsp",
                "textDocument/hover",
                "rust-analyzer",
                Some(1),
                None,
            ),
        ];
        let entries = pair_merge(&messages);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0],
            DisplayEntry::Paired {
                request_index: 0,
                response_index: 1,
                parent_id: None,
            }
        );
    }

    #[test]
    fn test_run_collapse_propagates_parent_id() {
        let messages = vec![
            {
                let mut m = make_progress_message("rust-analyzer", "ra/indexing");
                m.parent_id = Some(42);
                m
            },
            {
                let mut m = make_progress_message("rust-analyzer", "ra/indexing");
                m.parent_id = Some(42);
                m
            },
            {
                let mut m = make_progress_message("rust-analyzer", "ra/indexing");
                m.parent_id = Some(42);
                m
            },
        ];
        let entries = pair_merge(&messages);
        let collapsed = run_collapse(entries, &messages);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(
            collapsed[0],
            DisplayEntry::Collapsed {
                start_index: 0,
                end_index: 2,
                count: 3,
                parent_id: Some(42),
            }
        );
    }

    #[test]
    fn test_pair_merge_parent_id_from_request() {
        let messages = vec![
            make_message_with_id_parent(
                1,
                "lsp",
                "textDocument/hover",
                "rust-analyzer",
                None,
                Some(10),
            ),
            make_message_with_id_parent(
                2,
                "lsp",
                "textDocument/hover",
                "rust-analyzer",
                Some(1),
                Some(10),
            ),
        ];
        let entries = pair_merge(&messages);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0],
            DisplayEntry::Paired {
                request_index: 0,
                response_index: 1,
                parent_id: Some(10),
            }
        );
    }

    // ── Scope collapse tests ────────────────────────────────────────────

    #[test]
    fn test_scope_collapse_basic() {
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
            make_message_with_id_parent(
                4,
                "lsp",
                "textDocument/references",
                "rust-analyzer",
                None,
                Some(1),
            ),
            make_message_with_id_parent(5, "mcp", "tools/call", "catenary", Some(1), None),
        ];
        let merged = pair_merge(&messages);
        let scoped = scope_collapse(merged, &messages);
        assert_eq!(scoped.len(), 2, "expected scope + MCP response: {scoped:?}");
        match &scoped[0] {
            DisplayEntry::Scope {
                parent,
                children,
                position,
            } => {
                assert!(
                    matches!(*parent.as_ref(), DisplayEntry::Single { index: 0, .. }),
                    "parent should be Single(0)"
                );
                assert_eq!(children.len(), 3, "should have 3 children");
                assert_eq!(*position, SegmentPosition::Only);
            }
            other => panic!("expected Scope, got {other:?}"),
        }
        assert!(
            matches!(scoped[1], DisplayEntry::Single { index: 4, .. }),
            "MCP response should be passthrough"
        );
    }

    #[test]
    fn test_scope_collapse_no_parent_id() {
        let messages = vec![
            make_message_with_id_parent(1, "lsp", "$/progress", "rust-analyzer", None, None),
            make_message_with_id_parent(2, "lsp", "$/progress", "rust-analyzer", None, None),
            make_message_with_id_parent(3, "lsp", "$/progress", "rust-analyzer", None, None),
        ];
        let merged = pair_merge(&messages);
        let scoped = scope_collapse(merged, &messages);
        assert_eq!(scoped.len(), 3);
        for entry in &scoped {
            assert!(
                matches!(entry, DisplayEntry::Single { .. }),
                "all entries should be Single"
            );
        }
    }

    #[test]
    fn test_scope_collapse_preserves_order() {
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
            make_message_with_id_parent(3, "mcp", "tools/call", "catenary", None, None),
            make_message_with_id_parent(4, "lsp", "workspace/symbol", "taplo", None, Some(3)),
            make_message_with_id_parent(
                5,
                "lsp",
                "textDocument/references",
                "rust-analyzer",
                None,
                Some(1),
            ),
        ];
        let merged = pair_merge(&messages);
        let scoped = scope_collapse(merged, &messages);
        assert_eq!(scoped.len(), 2, "expected 2 scopes: {scoped:?}");
        match &scoped[0] {
            DisplayEntry::Scope {
                parent,
                children,
                position,
            } => {
                assert!(
                    matches!(*parent.as_ref(), DisplayEntry::Single { index: 0, .. }),
                    "first scope parent should be index 0"
                );
                assert_eq!(children.len(), 2, "scope A should have 2 children");
                assert_eq!(*position, SegmentPosition::Only);
            }
            other => panic!("expected Scope A, got {other:?}"),
        }
        match &scoped[1] {
            DisplayEntry::Scope {
                parent,
                children,
                position,
            } => {
                assert!(
                    matches!(*parent.as_ref(), DisplayEntry::Single { index: 2, .. }),
                    "second scope parent should be index 2"
                );
                assert_eq!(children.len(), 1, "scope B should have 1 child");
                assert_eq!(*position, SegmentPosition::Only);
            }
            other => panic!("expected Scope B, got {other:?}"),
        }
    }

    #[test]
    fn test_scope_collapse_root_level_unaffected() {
        let messages = vec![
            make_message_with_id_parent(1, "mcp", "initialize", "catenary", None, None),
            make_message_with_id_parent(
                2,
                "mcp",
                "notifications/initialized",
                "catenary",
                None,
                None,
            ),
        ];
        let merged = pair_merge(&messages);
        let scoped = scope_collapse(merged, &messages);
        assert_eq!(scoped.len(), 2);
        assert!(
            matches!(scoped[0], DisplayEntry::Single { index: 0, .. }),
            "initialize should be Single"
        );
        assert!(
            matches!(scoped[1], DisplayEntry::Single { index: 1, .. }),
            "initialized should be Single"
        );
    }

    // ── Segmented scope tests ──────────────────────────────────────────

    #[test]
    fn test_segmented_scope_one_interruption() {
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
            make_message_with_id_parent(4, "lsp", "$/progress", "rust-analyzer", None, None),
            make_message_with_id_parent(
                5,
                "lsp",
                "textDocument/references",
                "rust-analyzer",
                None,
                Some(1),
            ),
        ];
        let merged = pair_merge(&messages);
        let scoped = scope_collapse(merged, &messages);
        assert_eq!(
            scoped.len(),
            3,
            "expected 2 segments + 1 interruption: {scoped:?}"
        );
        match &scoped[0] {
            DisplayEntry::Scope {
                parent,
                children,
                position,
            } => {
                assert!(
                    matches!(*parent.as_ref(), DisplayEntry::Single { index: 0, .. }),
                    "parent should be Single(0)"
                );
                assert_eq!(children.len(), 2, "first segment should have 2 children");
                assert_eq!(*position, SegmentPosition::First);
            }
            other => panic!("expected First Scope, got {other:?}"),
        }
        assert!(
            matches!(scoped[1], DisplayEntry::Single { index: 3, .. }),
            "interruption should be Single(3): {:?}",
            scoped[1]
        );
        match &scoped[2] {
            DisplayEntry::Scope {
                parent,
                children,
                position,
            } => {
                assert!(
                    matches!(*parent.as_ref(), DisplayEntry::Single { index: 0, .. }),
                    "parent should be Single(0)"
                );
                assert_eq!(children.len(), 1, "last segment should have 1 child");
                assert_eq!(*position, SegmentPosition::Last);
            }
            other => panic!("expected Last Scope, got {other:?}"),
        }
        if let (DisplayEntry::Scope { parent: p1, .. }, DisplayEntry::Scope { parent: p2, .. }) =
            (&scoped[0], &scoped[2])
        {
            assert!(
                Rc::ptr_eq(p1, p2),
                "segments should share the same Rc parent"
            );
        }
    }

    #[test]
    fn test_segmented_scope_two_interruptions() {
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
            make_message_with_id_parent(3, "lsp", "$/progress", "rust-analyzer", None, None),
            make_message_with_id_parent(
                4,
                "lsp",
                "textDocument/references",
                "rust-analyzer",
                None,
                Some(1),
            ),
            make_message_with_id_parent(5, "lsp", "$/progress", "rust-analyzer", None, None),
            make_message_with_id_parent(
                6,
                "lsp",
                "textDocument/hover",
                "rust-analyzer",
                None,
                Some(1),
            ),
        ];
        let merged = pair_merge(&messages);
        let scoped = scope_collapse(merged, &messages);
        assert_eq!(
            scoped.len(),
            5,
            "expected 3 segments + 2 interruptions: {scoped:?}"
        );
        assert_eq!(
            match &scoped[0] {
                DisplayEntry::Scope { position, .. } => *position,
                other => panic!("expected Scope, got {other:?}"),
            },
            SegmentPosition::First
        );
        assert!(matches!(scoped[1], DisplayEntry::Single { index: 2, .. }));
        assert_eq!(
            match &scoped[2] {
                DisplayEntry::Scope { position, .. } => *position,
                other => panic!("expected Scope, got {other:?}"),
            },
            SegmentPosition::Middle
        );
        assert!(matches!(scoped[3], DisplayEntry::Single { index: 4, .. }));
        assert_eq!(
            match &scoped[4] {
                DisplayEntry::Scope { position, .. } => *position,
                other => panic!("expected Scope, got {other:?}"),
            },
            SegmentPosition::Last
        );
    }

    #[test]
    fn test_segmented_scope_no_interruption() {
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
        let merged = pair_merge(&messages);
        let scoped = scope_collapse(merged, &messages);
        assert_eq!(scoped.len(), 1, "expected single scope: {scoped:?}");
        match &scoped[0] {
            DisplayEntry::Scope {
                children, position, ..
            } => {
                assert_eq!(children.len(), 2);
                assert_eq!(*position, SegmentPosition::Only);
            }
            other => panic!("expected Only Scope, got {other:?}"),
        }
    }

    #[test]
    fn test_scope_collapse_nesting() {
        // Non-adjacent LSP req/resp under an MCP scope. The LSP request
        // is both a scope parent (response references it) and a child of
        // the MCP scope (its parent_id references the MCP request).
        let messages = vec![
            make_message_with_id_parent(1, "mcp", "tools/call", "catenary", None, None),
            // LSP request — child of MCP (parent_id=1), scope parent for LSP response
            make_message_with_id_parent(
                2,
                "lsp",
                "textDocument/hover",
                "rust-analyzer",
                None,
                Some(1),
            ),
            // Notification interrupts, preventing pair merge
            make_message_with_id_parent(3, "lsp", "$/progress", "rust-analyzer", None, Some(1)),
            // LSP response — parent_id=2 (its request), not adjacent so not pair-merged
            make_message_with_id_parent(
                4,
                "lsp",
                "textDocument/hover",
                "rust-analyzer",
                Some(2),
                Some(2),
            ),
        ];
        let merged = pair_merge(&messages);
        let scoped = scope_collapse(merged, &messages);

        // The MCP scope should contain the LSP scope as a nested child.
        assert_eq!(scoped.len(), 1, "expected 1 top-level scope: {scoped:?}");
        match &scoped[0] {
            DisplayEntry::Scope {
                parent,
                children,
                position,
            } => {
                assert!(
                    matches!(*parent.as_ref(), DisplayEntry::Single { index: 0, .. }),
                    "parent should be MCP request at index 0"
                );
                assert_eq!(*position, SegmentPosition::Only);
                // Children: the notification, plus the nested LSP scope
                assert!(
                    children.len() >= 2,
                    "MCP scope should have at least 2 children (notification + nested scope): {children:?}"
                );
                // At least one child should be a nested Scope
                let has_nested_scope = children
                    .iter()
                    .any(|c| matches!(c, DisplayEntry::Scope { .. }));
                assert!(
                    has_nested_scope,
                    "MCP scope should contain a nested LSP scope: {children:?}"
                );
            }
            other => panic!("expected Scope, got {other:?}"),
        }
    }

    /// Regression test: the full pipeline (`pair_merge` → `scope_collapse` →
    /// `run_collapse`) must produce identical output on repeated calls with
    /// the same input. Non-deterministic `HashMap` iteration in
    /// `scope_collapse` previously caused display jitter in the TUI.
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "test builds 12 messages for full pipeline coverage"
    )]
    fn test_pipeline_deterministic() {
        // Simulate a grep tool call with interleaved yaml-language-server
        // notifications — the exact scenario that produced jitter.
        let messages = vec![
            // hook pre-tool
            make_message_with_id_parent(1, "hook", "pre-tool/enforce-editing", "", None, None),
            // MCP grep request (scope parent)
            make_message_with_id_parent(2, "mcp", "tools/call", "", None, None),
            // LSP children of grep
            make_message_with_id_parent(
                3,
                "lsp",
                "textDocument/didOpen",
                "rust-analyzer",
                None,
                Some(2),
            ),
            make_message_with_id_parent(
                4,
                "lsp",
                "textDocument/hover",
                "rust-analyzer",
                None,
                Some(2),
            ),
            // yaml interruption
            make_message_with_id_parent(
                5,
                "lsp",
                "workspace/configuration",
                "yaml-language-server",
                None,
                None,
            ),
            make_message_with_id_parent(
                6,
                "lsp",
                "textDocument/publishDiagnostics",
                "yaml-language-server",
                None,
                None,
            ),
            // more LSP children of grep
            make_message_with_id_parent(
                7,
                "lsp",
                "textDocument/hover",
                "rust-analyzer",
                None,
                Some(2),
            ),
            make_message_with_id_parent(
                8,
                "lsp",
                "textDocument/hover",
                "rust-analyzer",
                None,
                Some(2),
            ),
            // another yaml interruption
            make_message_with_id_parent(
                9,
                "lsp",
                "workspace/configuration",
                "yaml-language-server",
                None,
                None,
            ),
            make_message_with_id_parent(
                10,
                "lsp",
                "textDocument/publishDiagnostics",
                "yaml-language-server",
                None,
                None,
            ),
            // final LSP children
            make_message_with_id_parent(
                11,
                "lsp",
                "textDocument/didClose",
                "rust-analyzer",
                None,
                Some(2),
            ),
            // hook post-tool
            make_message_with_id_parent(12, "hook", "post-tool/diagnostics", "", None, None),
        ];

        // Run the full pipeline 20 times and assert all runs produce
        // the same result.
        let reference = {
            let merged = pair_merge(&messages);
            let scoped = scope_collapse(merged, &messages);
            run_collapse(scoped, &messages)
        };

        for i in 1..20 {
            let merged = pair_merge(&messages);
            let scoped = scope_collapse(merged, &messages);
            let result = run_collapse(scoped, &messages);

            assert_eq!(
                reference.len(),
                result.len(),
                "run {i}: entry count differs ({} vs {})",
                reference.len(),
                result.len()
            );

            for (j, (a, b)) in reference.iter().zip(result.iter()).enumerate() {
                assert_eq!(
                    format!("{a:?}"),
                    format!("{b:?}"),
                    "run {i}, entry {j}: display entries differ"
                );
            }
        }
    }
}

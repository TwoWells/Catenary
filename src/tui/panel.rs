// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Messages panel: renders a list of protocol messages with cursor, scroll
//! offset, tail attach/detach behavior, and horizontal scroll indicators.
//!
//! This is the core building block — later tickets add expansion (04),
//! multi-panel grid (05), scrollbar (06), and selection (07) on top of this.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Widget};
use unicode_width::UnicodeWidthStr;

use super::category;
use super::format::{
    format_collapsed_plain, format_collapsed_styled, format_message_plain, format_message_styled,
    format_pair_plain, format_pair_styled, format_scope_plain, format_scope_styled,
};
use super::icons::IconSet;
use super::selection::VisualSelection;
use super::theme::Theme;
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
                children, parent, ..
            } => children
                .first()
                .map_or_else(|| parent.expansion_index(), Self::expansion_index),
        }
    }
}

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
    let mut builders: HashMap<usize, ScopeBuilder> = HashMap::new();
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

/// State for a single messages panel.
pub struct PanelState<'a> {
    /// Session ID this panel is tailing.
    pub session_id: String,
    /// All messages loaded for this session.
    pub messages: Vec<SessionMessage>,
    /// Cursor position (index into flat lines).
    pub cursor: usize,
    /// Scroll offset from top of content.
    pub scroll_offset: usize,
    /// Whether the panel is attached to the tail (auto-scrolling).
    pub tail_attached: bool,
    /// Horizontal scroll offset (for wide lines).
    pub horizontal_scroll: usize,
    /// Whether this panel is pinned (enlarged).
    pub pinned: bool,
    /// Language server names for the title bar.
    pub language_servers: Vec<String>,
    /// Indices of expanded messages (in the messages Vec).
    pub expanded: HashSet<usize>,
    /// Active visual selection, if any.
    pub visual_selection: Option<VisualSelection>,
    /// Last known viewport height (updated each render frame).
    pub viewport_height: usize,
    /// Display ID for the title bar (client session ID if available, else internal ID).
    pub display_id: String,
    /// Active filter pattern (case-insensitive substring match).
    pub filter_pattern: Option<String>,
    /// Semantic color theme (borrowed from the application).
    pub theme: &'a Theme,
    /// Resolved icon set (borrowed from the application).
    pub icons: &'a IconSet,
}

// ── Construction & navigation ───────────────────────────────────────────

impl<'a> PanelState<'a> {
    /// Create a new panel for the given session.
    ///
    /// Starts with empty messages, cursor at 0, tail attached, no horizontal
    /// scroll, not pinned.
    #[must_use]
    pub fn new(session_id: String, theme: &'a Theme, icons: &'a IconSet) -> Self {
        let display_id = session_id.clone();
        Self {
            session_id,
            messages: Vec::new(),
            cursor: 0,
            scroll_offset: 0,
            tail_attached: true,
            horizontal_scroll: 0,
            pinned: false,
            language_servers: Vec::new(),
            expanded: HashSet::new(),
            display_id,
            visual_selection: None,
            viewport_height: 0,
            filter_pattern: None,
            theme,
            icons,
        }
    }

    /// Total number of visible lines (flat lines including expanded detail).
    fn total_lines(&self) -> usize {
        self.flat_lines().len()
    }

    /// Load historical messages. Sets cursor to the last line and attaches tail.
    pub fn load_messages(&mut self, messages: Vec<SessionMessage>) {
        self.messages = messages;
        self.expanded.clear();
        let total = self.total_lines();
        self.cursor = total.saturating_sub(1);
        self.tail_attached = true;
        self.snap_viewport(0);
    }

    /// Append a new message.
    ///
    /// If tail attached, advance cursor and scroll to keep the latest message
    /// visible. If detached, just append (cursor stays put).
    pub fn push_message(&mut self, msg: SessionMessage) {
        self.messages.push(msg);
        if self.tail_attached {
            let total = self.total_lines();
            self.cursor = total.saturating_sub(1);
            self.snap_viewport(0);
        }
    }

    /// Move cursor by `delta`. Clamp to `[0, total_lines - 1]`.
    ///
    /// - Moving up (`delta < 0`): detach tail.
    /// - Moving down past the last line: reattach tail, cursor on last.
    /// - Snap viewport to center cursor (`scrolloff=999` behavior).
    #[allow(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        reason = "terminal item counts never overflow isize"
    )]
    pub fn navigate(&mut self, delta: isize) {
        let total = self.total_lines();
        if total == 0 {
            self.cursor = 0;
            return;
        }

        let max = (total - 1) as isize;
        let new_pos = self.cursor as isize + delta;

        if delta < 0 {
            self.tail_attached = false;
        }

        if new_pos > max {
            // Moved past end — reattach.
            self.cursor = total - 1;
            self.tail_attached = true;
        } else {
            self.cursor = new_pos.max(0) as usize;
        }

        self.snap_viewport(0);
    }

    /// Scroll viewport by `delta` lines without moving the cursor.
    ///
    /// Used for mouse wheel: moves `scroll_offset` only, detaches tail on
    /// scroll-up, reattaches when scrolled to the very bottom.
    #[allow(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        reason = "terminal item counts never overflow isize"
    )]
    pub fn scroll_viewport(&mut self, delta: isize) {
        let total = self.total_lines();
        if total == 0 {
            return;
        }

        if delta < 0 {
            self.tail_attached = false;
        }

        let new_offset = (self.scroll_offset as isize + delta)
            .max(0)
            .min(total.saturating_sub(1) as isize);

        #[allow(clippy::cast_sign_loss, reason = "clamped to >= 0")]
        {
            self.scroll_offset = new_offset as usize;
        }

        // Reattach tail if scrolled to the very bottom.
        let vh = if self.viewport_height > 0 {
            self.viewport_height
        } else {
            20
        };
        if self.scroll_offset + vh >= total {
            self.tail_attached = true;
        }
    }

    /// Jump to first line — `g` key.
    pub const fn scroll_to_top(&mut self) {
        self.cursor = 0;
        self.scroll_offset = 0;
        self.tail_attached = false;
    }

    /// Jump to last line — `G` key.
    pub fn scroll_to_bottom(&mut self) {
        let total = self.total_lines();
        self.cursor = total.saturating_sub(1);
        self.tail_attached = true;
        self.snap_viewport(0);
    }

    /// Page up — `Ctrl+U`.
    pub fn page_up(&mut self, height: usize) {
        let half = (height / 2).max(1);
        #[allow(
            clippy::cast_possible_wrap,
            reason = "terminal heights never overflow isize"
        )]
        self.navigate(-(half as isize));
    }

    /// Scroll horizontally by `delta` columns.
    ///
    /// Positive delta scrolls right, negative scrolls left. Clamps to zero.
    #[allow(
        clippy::cast_sign_loss,
        reason = "delta is checked positive before cast"
    )]
    pub const fn scroll_horizontal(&mut self, delta: isize) {
        if delta < 0 {
            self.horizontal_scroll = self.horizontal_scroll.saturating_sub(delta.unsigned_abs());
        } else {
            self.horizontal_scroll = self.horizontal_scroll.saturating_add(delta as usize);
        }
    }

    /// Page down — `Ctrl+D`.
    pub fn page_down(&mut self, height: usize) {
        let half = (height / 2).max(1);
        #[allow(
            clippy::cast_possible_wrap,
            reason = "terminal heights never overflow isize"
        )]
        self.navigate(half as isize);
    }

    /// Compute the `(start, end)` indices of lines visible in the viewport.
    ///
    /// `height` is the inner content height (excluding title bar and borders).
    #[must_use]
    pub fn visible_range(&self, height: usize) -> (usize, usize) {
        let total = self.total_lines();
        let start = self.scroll_offset.min(total);
        let end = (start + height).min(total);
        (start, end)
    }

    /// Derive language server names from messages.
    ///
    /// Scans for LSP messages and tracks unique server names.
    pub fn update_language_servers(&mut self) {
        let mut seen: HashSet<String> = HashSet::new();
        let mut order: Vec<String> = Vec::new();

        for msg in &self.messages {
            if msg.r#type == "lsp" && !msg.server.is_empty() && seen.insert(msg.server.clone()) {
                order.push(msg.server.clone());
            }
        }

        self.language_servers = order;
    }

    /// Snap viewport so cursor is centered (`scrolloff=999` behavior).
    ///
    /// `height` of 0 means use the last known viewport height from the
    /// previous render frame. Falls back to 20 if never rendered.
    fn snap_viewport(&mut self, height: usize) {
        let h = if height > 0 {
            height
        } else if self.viewport_height > 0 {
            self.viewport_height
        } else {
            20
        };
        let total = self.total_lines();
        if total <= h {
            self.scroll_offset = 0;
            return;
        }
        let target = self.cursor.saturating_sub(h / 2);
        self.scroll_offset = target.min(total.saturating_sub(h));
    }

    // ── Expansion ───────────────────────────────────────────────────────

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

    /// Toggle expansion of the message under the cursor.
    ///
    /// - On a `MessageHeader`: toggle the message in/out of `expanded`.
    /// - On a `Detail` line: collapse the parent message, move cursor to its header.
    pub fn toggle_expansion(&mut self) {
        let flat = self.flat_lines();
        let Some(current) = flat.get(self.cursor) else {
            return;
        };
        match *current {
            FlatLine::MessageHeader { message_index, .. } => {
                if self.expanded.contains(&message_index) {
                    self.expanded.remove(&message_index);
                } else {
                    self.expanded.insert(message_index);
                }
            }
            FlatLine::Detail { message_index, .. } => {
                self.expanded.remove(&message_index);
                // Move cursor to the parent header.
                let new_flat = self.flat_lines();
                if let Some(pos) = new_flat.iter().position(|fl| {
                    matches!(fl, FlatLine::MessageHeader { message_index: mi, .. } if *mi == message_index)
                }) {
                    self.cursor = pos;
                }
            }
            FlatLine::CollapsedHeader { start_index, .. } => {
                if self.expanded.contains(&start_index) {
                    self.expanded.remove(&start_index);
                } else {
                    self.expanded.insert(start_index);
                }
            }
            FlatLine::ScopeHeader { expansion_key, .. } => {
                if self.expanded.contains(&expansion_key) {
                    self.expanded.remove(&expansion_key);
                } else {
                    self.expanded.insert(expansion_key);
                }
            }
            FlatLine::ScopeChild {
                scope_parent_index, ..
            } => {
                // Collapse the parent scope segment and move cursor to its header.
                self.expanded.remove(&scope_parent_index);
                let new_flat = self.flat_lines();
                if let Some(pos) = new_flat.iter().position(|fl| {
                    matches!(fl, FlatLine::ScopeHeader { expansion_key, .. } if *expansion_key == scope_parent_index)
                }) {
                    self.cursor = pos;
                }
            }
        }
        self.snap_viewport(0);
    }
}

// ── Expansion helpers ───────────────────────────────────────────────────

/// Generate styled detail lines for an expanded message.
///
/// Returns an empty vec for messages with empty payloads.
#[must_use]
pub fn detail_lines(msg: &SessionMessage, theme: &Theme) -> Vec<Line<'static>> {
    let payload = &msg.payload;
    if payload.as_object().is_none_or(serde_json::Map::is_empty) {
        return Vec::new();
    }

    // Indent to align past the timestamp column ("HH:MM:SS  " = 10 chars).
    let indent = "          ";
    let mut lines = Vec::new();

    // Line 1: method [type]
    lines.push(Line::from(vec![
        Span::raw(indent.to_string()),
        Span::styled(format!("{} [{}]", msg.method, msg.r#type), theme.muted),
    ]));

    // Line 2: separator
    lines.push(Line::from(vec![
        Span::raw(indent.to_string()),
        Span::styled("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}", theme.muted),
    ]));

    // Lines 3+: pretty-printed payload
    if let Ok(pretty) = serde_json::to_string_pretty(payload) {
        for line in pretty.lines() {
            lines.push(Line::from(vec![
                Span::raw(indent.to_string()),
                Span::styled(line.to_string(), theme.muted),
            ]));
        }
    }

    lines
}

/// Generate styled detail lines for an expanded request/response pair.
///
/// Shows the request payload under a `→` header and the response payload
/// under a `←` header.
#[must_use]
pub fn pair_detail_lines(
    request: &SessionMessage,
    response: &SessionMessage,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let indent = "          ";
    let mut lines = Vec::new();

    // Request section
    let req_payload = &request.payload;
    if req_payload.as_object().is_some_and(|o| !o.is_empty()) {
        lines.push(Line::from(vec![
            Span::raw(indent.to_string()),
            Span::styled(
                format!("\u{2192} {} [{}]", request.method, request.r#type),
                theme.muted,
            ),
        ]));
        lines.push(Line::from(vec![
            Span::raw(indent.to_string()),
            Span::styled(
                "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
                theme.muted,
            ),
        ]));
        if let Ok(pretty) = serde_json::to_string_pretty(req_payload) {
            for line in pretty.lines() {
                lines.push(Line::from(vec![
                    Span::raw(indent.to_string()),
                    Span::styled(line.to_string(), theme.muted),
                ]));
            }
        }
    }

    // Response section
    let resp_payload = &response.payload;
    if resp_payload.as_object().is_some_and(|o| !o.is_empty()) {
        lines.push(Line::from(vec![
            Span::raw(indent.to_string()),
            Span::styled(
                format!("\u{2190} {} [{}]", response.method, response.r#type),
                theme.muted,
            ),
        ]));
        lines.push(Line::from(vec![
            Span::raw(indent.to_string()),
            Span::styled(
                "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
                theme.muted,
            ),
        ]));
        if let Ok(pretty) = serde_json::to_string_pretty(resp_payload) {
            for line in pretty.lines() {
                lines.push(Line::from(vec![
                    Span::raw(indent.to_string()),
                    Span::styled(line.to_string(), theme.muted),
                ]));
            }
        }
    }

    lines
}

// ── Rendering ───────────────────────────────────────────────────────────

/// Build the title line for a panel.
fn build_title<'a>(state: &'a PanelState<'a>) -> Line<'a> {
    let id_short = if state.display_id.len() > 8 {
        &state.display_id[..8]
    } else {
        &state.display_id
    };

    let mut spans = vec![Span::raw(format!(" Events [{id_short}]"))];

    if state.language_servers.is_empty() {
        spans.push(Span::styled(" no ls", Style::default().fg(Color::DarkGray)));
    } else {
        let style = Style::default().fg(Color::Green);
        spans.push(Span::raw(" "));
        for (i, name) in state.language_servers.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" \u{2571} ")); // ╱
            }
            spans.push(Span::styled(state.icons.ls_active.as_str(), style));
            spans.push(Span::styled(name.as_str(), style));
        }
    }

    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// Convert a borrowed `Line` into a fully owned `Line<'static>`.
#[must_use]
pub fn to_owned_line(line: &Line<'_>) -> Line<'static> {
    Line::from(
        line.spans
            .iter()
            .map(|s| Span::styled(s.content.to_string(), s.style))
            .collect::<Vec<_>>(),
    )
}

/// Apply horizontal scrolling to a styled line, inserting clip indicators.
///
/// Returns a new line clipped to `width` display columns, with `◀…` on the
/// left when content is clipped left and `…▶` on the right when clipped right.
fn clip_line_horizontal(line: &Line<'_>, h_scroll: usize, width: usize) -> Line<'static> {
    if width < 4 {
        // Too narrow for indicators, just return empty.
        return Line::default();
    }

    // Flatten the line into a single plain string + collect grapheme-aware info.
    let full_text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let full_width = UnicodeWidthStr::width(full_text.as_str());

    if full_width == 0 {
        return Line::default();
    }

    // No clipping needed at all.
    if h_scroll == 0 && full_width <= width {
        return to_owned_line(line);
    }

    let clipped_left = h_scroll > 0;
    let clipped_right = full_width > h_scroll + width;

    // Calculate available content width after reserving indicator space.
    let left_reserve = if clipped_left { 2 } else { 0 };
    let right_reserve = if clipped_right { 2 } else { 0 };
    let content_width = width.saturating_sub(left_reserve + right_reserve);

    if content_width == 0 {
        let mut spans = Vec::new();
        if clipped_left {
            spans.push(Span::styled(
                "\u{25C0}\u{2026}",
                Style::default().fg(Color::DarkGray),
            ));
        }
        if clipped_right {
            spans.push(Span::styled(
                "\u{2026}\u{25B6}",
                Style::default().fg(Color::DarkGray),
            ));
        }
        return Line::from(spans);
    }

    // Walk through spans, tracking display-width position, and extract the
    // visible portion respecting h_scroll and content_width.
    let vis_start = h_scroll;
    let vis_end = h_scroll + content_width;

    let mut result_spans: Vec<Span<'static>> = Vec::new();

    if clipped_left {
        result_spans.push(Span::styled(
            "\u{25C0}\u{2026}",
            Style::default().fg(Color::DarkGray),
        )); // ◀…
    }

    // Walk spans and extract visible portion.
    let mut col = 0usize;
    for span in &line.spans {
        let span_text = span.content.as_ref();
        let span_width = UnicodeWidthStr::width(span_text);
        let span_end = col + span_width;

        if span_end <= vis_start || col >= vis_end {
            // Entirely outside visible range.
            col = span_end;
            continue;
        }

        // Partially or fully visible. Extract the visible chars.
        let mut visible = String::new();
        let mut char_col = col;
        for ch in span_text.chars() {
            let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            let ch_end = char_col + ch_width;
            if char_col >= vis_end {
                break;
            }
            if ch_end > vis_start {
                visible.push(ch);
            }
            char_col = ch_end;
        }

        if !visible.is_empty() {
            result_spans.push(Span::styled(visible, span.style));
        }

        col = span_end;
    }

    if clipped_right {
        result_spans.push(Span::styled(
            "\u{2026}\u{25B6}",
            Style::default().fg(Color::DarkGray),
        )); // …▶
    }

    Line::from(result_spans)
}

/// Render a single `FlatLine` into a styled `Line`.
///
/// Shared between the top-level render loop and `ScopeChild` indentation.
fn render_flat_line_styled(
    fl: &FlatLine,
    all_flat: &[FlatLine],
    state: &PanelState<'_>,
    detail_cache: &mut HashMap<usize, Vec<Line<'static>>>,
) -> Line<'static> {
    match fl {
        FlatLine::MessageHeader {
            message_index,
            paired_response,
        } => paired_response.map_or_else(
            || format_message_styled(&state.messages[*message_index], state.icons, state.theme),
            |resp_idx| {
                format_pair_styled(
                    &state.messages[*message_index],
                    &state.messages[resp_idx],
                    state.icons,
                    state.theme,
                )
            },
        ),
        FlatLine::Detail {
            message_index,
            detail_index,
        } => detail_cache
            .entry(*message_index)
            .or_insert_with(|| {
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
                resp_idx.map_or_else(
                    || detail_lines(&state.messages[*message_index], state.theme),
                    |ri| {
                        pair_detail_lines(
                            &state.messages[*message_index],
                            &state.messages[ri],
                            state.theme,
                        )
                    },
                )
            })
            .get(*detail_index)
            .cloned()
            .unwrap_or_default(),
        FlatLine::CollapsedHeader {
            start_index,
            end_index,
            count,
        } => format_collapsed_styled(
            &state.messages,
            *start_index,
            *end_index,
            *count,
            state.icons,
            state.theme,
        ),
        FlatLine::ScopeHeader {
            parent,
            child_count,
            position,
            ..
        } => format_scope_styled(
            parent,
            *child_count,
            *position,
            &state.messages,
            state.icons,
            state.theme,
        ),
        FlatLine::ScopeChild { depth, inner, .. } => {
            let indent = " ".repeat(depth * 4);
            let inner_line = render_flat_line_styled(inner, all_flat, state, detail_cache);
            let mut spans = vec![Span::raw(indent)];
            spans.extend(
                inner_line
                    .spans
                    .into_iter()
                    .map(|s| Span::styled(s.content.into_owned(), s.style)),
            );
            Line::from(spans)
        }
    }
}

/// Render a single messages panel into the given buffer area.
///
/// The panel owns its top row (title bar) and right column (scrollbar,
/// rendered by ticket 06). Left and bottom edges are content. The caller
/// (grid) handles junction characters.
#[allow(
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    reason = "terminal coordinates are always small; pair merge adds detail lookup logic"
)]
pub fn render_panel(state: &PanelState<'_>, area: Rect, buf: &mut Buffer, focused: bool) {
    if area.width < 4 || area.height < 2 {
        return;
    }

    let border_style = if focused {
        state.theme.border_focused
    } else {
        state.theme.border_unfocused
    };

    let title_style = if focused {
        state.theme.title
    } else {
        state.theme.border_unfocused
    };

    let border_set = if focused {
        symbols::border::THICK
    } else {
        symbols::border::PLAIN
    };

    let title = build_title(state);
    let block = Block::default()
        .borders(Borders::TOP | Borders::RIGHT)
        .border_set(border_set)
        .border_style(border_style)
        .title(title)
        .title_style(title_style);
    let inner = block.inner(area);
    block.render(area, buf);

    if inner.width < 2 || inner.height < 1 {
        return;
    }

    // Build flat line list (headers + expanded detail lines).
    let flat = state.flat_lines();

    // Viewport slicing.
    let height = inner.height as usize;
    let total = flat.len();
    let start = state.scroll_offset.min(total);
    let end = (start + height).min(total);

    // Cache detail lines per expanded message to avoid recomputation.
    let mut detail_cache: HashMap<usize, Vec<Line<'static>>> = HashMap::new();

    // Render each visible line.
    let content_width = inner.width as usize;
    for (i, fl) in flat[start..end].iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }

        let line = render_flat_line_styled(fl, &flat, state, &mut detail_cache);

        let display_line = if state.horizontal_scroll > 0
            || UnicodeWidthStr::width(
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .as_str(),
            ) > content_width
        {
            clip_line_horizontal(&line, state.horizontal_scroll, content_width)
        } else {
            to_owned_line(&line)
        };

        // Apply cursor highlight to the entire row.
        let line_index = start + i;
        if line_index == state.cursor {
            // Set selection style on the entire row first.
            for x in inner.x..inner.x + inner.width {
                buf[(x, y)].set_style(state.theme.selection);
            }
        }

        buf.set_line(inner.x, y, &display_line, inner.width);

        // Re-apply selection style on top of content for cursor row.
        if line_index == state.cursor {
            for x in inner.x..inner.x + inner.width {
                buf[(x, y)].set_style(state.theme.selection);
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::config::IconConfig;
    use crate::session::SessionMessage;

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

    /// Create N messages that never collapse (hook messages have `None` collapse key).
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

    /// An LSP message with a non-empty payload (expandable).
    fn make_lsp_message() -> SessionMessage {
        make_message_with_payload(
            "lsp",
            "textDocument/hover",
            "rust-analyzer",
            serde_json::json!({"id": 1, "method": "textDocument/hover", "params": {}}),
        )
    }

    /// A hook diagnostic message with preview (expandable).
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

    #[test]
    fn test_panel_new_tail_attached() {
        let theme = test_theme();
        let icons = test_icons();
        let panel = PanelState::new("abc123".to_string(), &theme, &icons);
        assert!(panel.tail_attached);
        assert_eq!(panel.cursor, 0);
        assert_eq!(panel.scroll_offset, 0);
        assert_eq!(panel.horizontal_scroll, 0);
        assert!(!panel.pinned);
        assert!(panel.messages.is_empty());
    }

    #[test]
    fn test_panel_load_messages() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(10);
        panel.load_messages(messages);
        assert_eq!(panel.messages.len(), 10);
        assert_eq!(panel.cursor, 9);
        assert!(panel.tail_attached);
    }

    #[test]
    fn test_panel_push_message_attached() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(5);
        panel.load_messages(messages);
        assert_eq!(panel.cursor, 4);

        panel.push_message(make_message("mcp", "tools/list", "catenary"));
        assert_eq!(panel.messages.len(), 6);
        assert_eq!(panel.cursor, 5);
        assert!(panel.tail_attached);
    }

    #[test]
    fn test_panel_push_message_detached() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(5);
        panel.load_messages(messages);

        // Navigate up to detach.
        panel.navigate(-1);
        assert!(!panel.tail_attached);
        let cursor_before = panel.cursor;

        panel.push_message(make_message("mcp", "tools/list", "catenary"));
        assert_eq!(panel.messages.len(), 6);
        assert_eq!(panel.cursor, cursor_before);
        assert!(!panel.tail_attached);
    }

    #[test]
    fn test_panel_navigate_up_detaches() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(10);
        panel.load_messages(messages);
        assert_eq!(panel.cursor, 9);
        assert!(panel.tail_attached);

        panel.navigate(-1);
        assert_eq!(panel.cursor, 8);
        assert!(!panel.tail_attached);
    }

    #[test]
    fn test_panel_navigate_down_past_end_reattaches() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(5);
        panel.load_messages(messages);

        // Navigate up to detach.
        panel.navigate(-2);
        assert_eq!(panel.cursor, 2);
        assert!(!panel.tail_attached);

        // Navigate down past the end.
        panel.navigate(1);
        assert_eq!(panel.cursor, 3);
        panel.navigate(1);
        assert_eq!(panel.cursor, 4);
        // One more should clamp and reattach.
        panel.navigate(1);
        assert_eq!(panel.cursor, 4);
        assert!(panel.tail_attached);
    }

    #[test]
    fn test_panel_scroll_to_top() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(20);
        panel.load_messages(messages);

        panel.scroll_to_top();
        assert_eq!(panel.cursor, 0);
        assert_eq!(panel.scroll_offset, 0);
        assert!(!panel.tail_attached);
    }

    #[test]
    fn test_panel_scroll_to_bottom() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(20);
        panel.load_messages(messages);

        panel.scroll_to_top();
        assert!(!panel.tail_attached);

        panel.scroll_to_bottom();
        assert_eq!(panel.cursor, 19);
        assert!(panel.tail_attached);
    }

    #[test]
    fn test_panel_visible_range() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(100);
        panel.load_messages(messages);

        // Move cursor to 50 and snap viewport.
        panel.cursor = 50;
        panel.snap_viewport(20);

        let (start, end) = panel.visible_range(20);
        // Cursor at 50, centered in height 20 → offset ~40.
        assert_eq!(start, 40);
        assert_eq!(end, 60);
    }

    #[test]
    fn test_panel_visible_range_at_top() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(100);
        panel.load_messages(messages);

        // Cursor near top — can't center.
        panel.cursor = 3;
        panel.snap_viewport(20);

        let (start, end) = panel.visible_range(20);
        assert_eq!(start, 0);
        assert_eq!(end, 20);
    }

    #[test]
    fn test_panel_visible_range_at_bottom() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(100);
        panel.load_messages(messages);

        // Cursor near bottom.
        panel.cursor = 97;
        panel.snap_viewport(20);

        let (start, end) = panel.visible_range(20);
        assert_eq!(end, 100);
        assert_eq!(start, 80);
    }

    #[test]
    fn test_panel_render_messages() {
        let theme = test_theme();
        let icons = test_icons();
        // Use a hook message in between to break any potential collapse.
        let messages: Vec<SessionMessage> = vec![
            make_message_with_payload(
                "mcp",
                "tools/call",
                "catenary",
                serde_json::json!({"params": {"name": "grep"}}),
            ),
            make_message("hook", "break", "catenary"),
            make_message_with_payload(
                "mcp",
                "tools/call",
                "catenary",
                serde_json::json!({"params": {"name": "glob"}}),
            ),
        ];

        let mut panel = PanelState::new("test1234".to_string(), &theme, &icons);
        panel.load_messages(messages);

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_panel(&panel, area, f.buffer_mut(), true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(content.contains("grep"), "expected grep tool name");
        assert!(content.contains("glob"), "expected glob tool name");
    }

    #[test]
    fn test_panel_render_empty() {
        let theme = test_theme();
        let icons = test_icons();
        let panel = PanelState::new("empty123".to_string(), &theme, &icons);

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_panel(&panel, area, f.buffer_mut(), true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        // Should contain the title, no panic.
        assert!(content.contains("Events"), "expected title in empty panel");
    }

    #[test]
    fn test_panel_render_cursor_highlight() {
        let theme = test_theme();
        let icons = test_icons();
        let messages = make_non_collapsing_messages(5);

        let mut panel = PanelState::new("test1234".to_string(), &theme, &icons);
        panel.load_messages(messages);
        // Set cursor to row 1 (second message in visible area).
        panel.cursor = 1;
        panel.snap_viewport(8);

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_panel(&panel, area, f.buffer_mut(), true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();

        // The cursor row (row 1 in content = y=2 in buffer since y=0 is border,
        // y=1 is first content row, y=2 is second content row).
        // With cursor at index 1 and scroll_offset 0, cursor is at visible row 1.
        // Inner area starts at y=1 (after top border), so cursor row is at y=2.
        let cursor_y = 2u16;
        let inner_x = 1u16; // after left border
        let cell = &buf[(inner_x, cursor_y)];
        // The selection style uses REVERSED modifier.
        assert!(
            cell.modifier.contains(ratatui::style::Modifier::REVERSED),
            "expected REVERSED modifier on cursor row"
        );
    }

    #[test]
    fn test_panel_language_server_status() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("abc123".to_string(), &theme, &icons);
        panel.messages = vec![
            make_message("lsp", "textDocument/hover", "rust-analyzer"),
            make_message(
                "lsp",
                "textDocument/completion",
                "typescript-language-server",
            ),
        ];

        panel.update_language_servers();
        assert_eq!(panel.language_servers.len(), 2);
        assert_eq!(panel.language_servers[0], "rust-analyzer");
        assert_eq!(panel.language_servers[1], "typescript-language-server");
    }

    // ── Expansion tests ─────────────────────────────────────────────────

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
        // 3 headers + detail lines for the expanded hook message
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
    fn test_toggle_expansion_header() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let messages = vec![
            make_message("lsp", "initialized", "rust-analyzer"),
            make_lsp_message(),
        ];
        panel.load_messages(messages);
        // Cursor on message 1 (the expandable LSP message).
        panel.cursor = 1;

        panel.toggle_expansion();
        assert!(panel.expanded.contains(&1));

        panel.toggle_expansion();
        assert!(!panel.expanded.contains(&1));
    }

    #[test]
    fn test_toggle_expansion_detail() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let messages = vec![
            make_message("lsp", "initialized", "rust-analyzer"),
            make_lsp_message(),
            make_message("mcp", "tools/list", "catenary"),
        ];
        panel.load_messages(messages);
        panel.expanded.insert(1);
        // Find a detail line index.
        let flat = panel.flat_lines();
        let detail_pos = flat
            .iter()
            .position(|fl| {
                matches!(
                    fl,
                    FlatLine::Detail {
                        message_index: 1,
                        ..
                    }
                )
            })
            .expect("should have detail lines");
        panel.cursor = detail_pos;

        panel.toggle_expansion();
        assert!(!panel.expanded.contains(&1));
        // After collapse: cursor should be on message 1's header.
        assert_eq!(panel.cursor, 1);
    }

    #[test]
    fn test_toggle_expansion_empty_payload() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        // Empty payload — expansion is allowed but produces zero detail lines.
        let messages = vec![make_message("lsp", "initialized", "rust-analyzer")];
        panel.load_messages(messages);
        panel.cursor = 0;

        panel.toggle_expansion();
        assert!(panel.expanded.contains(&0));
        // Flat lines: header only (no detail lines for empty payload).
        let flat = panel.flat_lines();
        assert_eq!(flat.len(), 1);
    }

    #[test]
    fn test_cursor_walks_detail_lines() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let messages = vec![
            make_message("lsp", "initialized", "rust-analyzer"),
            make_lsp_message(),
            make_message("mcp", "tools/list", "catenary"),
        ];
        panel.load_messages(messages);
        panel.expanded.insert(1);

        let flat = panel.flat_lines();
        panel.cursor = 0;
        panel.tail_attached = false;

        // Walk through all lines one by one.
        for expected in flat.iter().skip(1) {
            panel.navigate(1);
            let current_flat = panel.flat_lines();
            assert_eq!(current_flat[panel.cursor], *expected);
        }
    }

    #[test]
    fn test_detail_lines_non_empty_payload() {
        let msg = make_lsp_message();
        let theme = test_theme();

        let lines = detail_lines(&msg, &theme);
        // Should have: method [type] header + separator + payload lines
        assert!(lines.len() >= 3, "should have header + sep + payload");
        let hdr: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            hdr.contains("textDocument/hover"),
            "header should contain method"
        );
        assert!(hdr.contains("[lsp]"), "header should contain type");
        let sep: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(sep.contains("\u{2500}"), "second line should be separator");
    }

    #[test]
    fn test_detail_lines_empty_payload() {
        let msg = make_message("lsp", "initialized", "rust-analyzer");
        let theme = test_theme();

        let lines = detail_lines(&msg, &theme);
        assert!(
            lines.is_empty(),
            "empty payload should have no detail lines"
        );
    }

    #[test]
    fn test_visible_range_with_expansion() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let mut messages = make_non_collapsing_messages(10);
        // Replace message 5 with an expandable one.
        messages[5] = make_lsp_message();
        panel.load_messages(messages);
        panel.expanded.insert(5);

        let flat = panel.flat_lines();
        let detail_count = detail_lines(&panel.messages[5], &theme).len();
        // 10 headers + detail lines for message 5
        assert_eq!(flat.len(), 10 + detail_count);

        // Set cursor to 0, snap viewport.
        panel.cursor = 0;
        panel.snap_viewport(10);
        let (start, end) = panel.visible_range(10);
        assert_eq!(start, 0);
        assert_eq!(end, 10);
    }

    #[test]
    fn test_render_expanded_message() {
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![make_hook_diag_message("/src/lib.rs", 2)];

        let mut panel = PanelState::new("test1234".to_string(), &theme, &icons);
        panel.load_messages(messages);
        panel.expanded.insert(0);
        panel.cursor = 0;
        panel.snap_viewport(8);

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_panel(&panel, area, f.buffer_mut(), true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        // Header should show the diagnostics summary.
        assert!(content.contains("lib.rs"), "expected file name in header");
        // Detail lines should contain the payload.
        assert!(content.contains("post-tool"), "expected method in detail");
    }

    // ── Pair merge tests ───────────────────────────────────────────────

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

    fn make_message_with_id_ts(
        id: i64,
        r#type: &str,
        method: &str,
        server: &str,
        request_id: Option<i64>,
        timestamp: chrono::DateTime<chrono::Utc>,
        payload: serde_json::Value,
    ) -> SessionMessage {
        SessionMessage {
            id,
            r#type: r#type.to_string(),
            method: method.to_string(),
            server: server.to_string(),
            client: "catenary".to_string(),
            request_id,
            parent_id: None,
            timestamp,
            payload,
        }
    }

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

    #[test]
    fn test_pair_merge_cancellation() {
        let messages = vec![
            make_message_with_id(1, "mcp", "tools/call", "catenary", None),
            make_message_with_id(2, "mcp", "notifications/cancelled", "catenary", Some(1)),
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

        // Verify rendering uses x-> arrow.
        let theme = test_theme();
        let icons = test_icons();
        let line =
            super::super::format::format_pair_styled(&messages[0], &messages[1], &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("x->"), "cancellation should show x-> arrow");
    }

    #[test]
    fn test_format_pair_styled_timing() {
        use chrono::{TimeDelta, Utc};

        let now = Utc::now();
        let later = now + TimeDelta::milliseconds(1500);
        let theme = test_theme();
        let icons = test_icons();

        let request = make_message_with_id_ts(
            1,
            "lsp",
            "textDocument/hover",
            "rust-analyzer",
            None,
            now,
            serde_json::json!({}),
        );
        let response = make_message_with_id_ts(
            2,
            "lsp",
            "textDocument/hover",
            "rust-analyzer",
            Some(1),
            later,
            serde_json::json!({"result": null}),
        );

        let line = super::super::format::format_pair_styled(&request, &response, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("1.5s"), "should contain timing delta: {text}");
    }

    #[test]
    fn test_format_pair_styled_lsp() {
        let theme = test_theme();
        let icons = test_icons();

        let request = make_message_with_id(1, "lsp", "textDocument/hover", "rust-analyzer", None);
        let mut response =
            make_message_with_id(2, "lsp", "textDocument/hover", "rust-analyzer", Some(1));
        response.payload = serde_json::json!({"result": {"contents": "fn main()"}});

        let line = super::super::format::format_pair_styled(&request, &response, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("[rust-analyzer]"),
            "should contain server name: {text}"
        );
        assert!(text.contains("<->"), "should contain <-> arrow: {text}");
        assert!(
            text.contains("textDocument/hover"),
            "should contain method: {text}"
        );
        assert!(text.contains("ok"), "should contain ok result: {text}");
    }

    #[test]
    fn test_format_pair_styled_mcp() {
        let theme = test_theme();
        let icons = test_icons();

        let request = make_message_with_id_ts(
            1,
            "mcp",
            "tools/call",
            "catenary",
            None,
            chrono::Utc::now(),
            serde_json::json!({"params": {"name": "grep"}}),
        );
        let response = make_message_with_id_ts(
            2,
            "mcp",
            "tools/call",
            "catenary",
            Some(1),
            chrono::Utc::now(),
            serde_json::json!({"result": {"content": [{"type": "text", "text": "results"}]}}),
        );

        let line = super::super::format::format_pair_styled(&request, &response, &icons, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("grep"), "should contain tool name: {text}");
        assert!(text.contains("ok"), "should contain result status: {text}");
    }

    // ── Run collapse tests ───────────────────────────────────────────────

    fn make_progress_message(server: &str, token: &str) -> SessionMessage {
        make_message_with_payload(
            "lsp",
            "$/progress",
            server,
            serde_json::json!({"token": token}),
        )
    }

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
        // MCP tools/call (id=1), three LSP children with parent_id=1,
        // MCP response (request_id=1). After pair merge + scope collapse:
        // one Scope entry containing the parent pair and three children.
        let messages = vec![
            // MCP request (id=1)
            make_message_with_id_parent(1, "mcp", "tools/call", "catenary", None, None),
            // LSP child 1 (parent_id=1)
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(1),
            ),
            // LSP child 2 (parent_id=1)
            make_message_with_id_parent(3, "lsp", "workspace/symbol", "taplo", None, Some(1)),
            // LSP child 3 (parent_id=1)
            make_message_with_id_parent(
                4,
                "lsp",
                "textDocument/references",
                "rust-analyzer",
                None,
                Some(1),
            ),
            // MCP response (request_id=1, so pairs with id=1)
            make_message_with_id_parent(5, "mcp", "tools/call", "catenary", Some(1), None),
        ];
        let merged = pair_merge(&messages);
        // pair merge: Paired(0,4) because msg[4].request_id != msg[3].id,
        // but msg[1].request_id is None so no pair there. Actually:
        // msg[0].id=1, msg[1].request_id=None → no pair. Singles 0..4, then
        // msg[4].request_id=Some(1) != msg[3].id=4 → no pair either.
        // So we get 5 singles. Let's verify.
        // Actually pair_merge only merges *adjacent* pairs. msg[4].request_id=Some(1),
        // msg[3].id=4, so no pair. All 5 are singles.

        let scoped = scope_collapse(merged, &messages);
        // msg[0] (id=1) is referenced by msg[1..=3] (parent_id=1) → scope parent
        // msg[4] has no parent_id → passthrough
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
        // Messages without parent_id pass through unchanged.
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
        // Two tool calls interleaved. Each scope contains only its own children.
        let messages = vec![
            // Tool call A (id=1)
            make_message_with_id_parent(1, "mcp", "tools/call", "catenary", None, None),
            // Child of A
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(1),
            ),
            // Tool call B (id=3)
            make_message_with_id_parent(3, "mcp", "tools/call", "catenary", None, None),
            // Child of B
            make_message_with_id_parent(4, "lsp", "workspace/symbol", "taplo", None, Some(3)),
            // Another child of A
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
        // Scope A at position 0 (with children msg[1] and msg[4]),
        // Scope B at position 2 (with child msg[3]).
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
        // MCP initialize / notifications/initialized have no parent_id,
        // remain as root entries.
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

    #[test]
    fn test_scope_flat_lines_collapsed() {
        // A scope that is not expanded produces a single ScopeHeader line.
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
        // Scope with 2 children → 1 ScopeHeader (collapsed)
        assert_eq!(flat.len(), 1, "collapsed scope should be 1 line: {flat:?}");
        assert!(
            matches!(flat[0], FlatLine::ScopeHeader { child_count: 2, .. }),
            "should be ScopeHeader with 2 children: {:?}",
            flat[0]
        );
    }

    #[test]
    fn test_scope_flat_lines_expanded() {
        // An expanded scope produces a ScopeHeader followed by ScopeChild entries.
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
        // Expand the scope (key is first child's message index = 1).
        panel.expanded.insert(1);

        let flat = panel.flat_lines();
        // ScopeHeader + 2 ScopeChild entries
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

    #[test]
    fn test_scope_toggle_expansion() {
        // Toggle on ScopeHeader adds/removes from expanded.
        // Toggle on ScopeChild collapses parent, moves cursor.
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
        // Cursor on the ScopeHeader (line 0).
        panel.cursor = 0;

        // Toggle: expand scope (expansion key is first child's index = 1).
        panel.toggle_expansion();
        assert!(
            panel.expanded.contains(&1),
            "scope should be expanded after toggle"
        );
        let flat = panel.flat_lines();
        assert_eq!(flat.len(), 3, "expanded should show 3 lines");

        // Toggle again: collapse scope.
        panel.cursor = 0;
        panel.toggle_expansion();
        assert!(
            !panel.expanded.contains(&1),
            "scope should be collapsed after second toggle"
        );

        // Expand again, then toggle on a child.
        panel.cursor = 0;
        panel.toggle_expansion();
        assert!(panel.expanded.contains(&1));
        // Move cursor to first ScopeChild (line 1).
        panel.cursor = 1;
        panel.toggle_expansion();
        assert!(
            !panel.expanded.contains(&1),
            "toggling on ScopeChild should collapse parent"
        );
        assert_eq!(
            panel.cursor, 0,
            "cursor should move to ScopeHeader after child toggle"
        );
    }

    #[test]
    fn test_scope_render_basic() {
        // Render a panel with a scope. Verify the tool name appears in the output.
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            {
                let mut m =
                    make_message_with_id_parent(1, "mcp", "tools/call", "catenary", None, None);
                m.payload = serde_json::json!({"params": {"name": "grep"}});
                m
            },
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

        let mut panel = PanelState::new("test1234".to_string(), &theme, &icons);
        panel.load_messages(messages);

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                render_panel(&panel, area, f.buffer_mut(), true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let content = buffer_to_string(&buf);

        assert!(
            content.contains("grep"),
            "expected grep tool name in scope header: {content}"
        );
        assert!(
            content.contains("2 children"),
            "expected child count in scope header: {content}"
        );
    }

    // ── Segmented scope tests ──────────────────────────────────────────

    #[test]
    fn test_segmented_scope_one_interruption() {
        // Tool call with 2 children, 1 root interruption, 1 more child.
        // Produces two segments: First, Last.
        let messages = vec![
            // MCP request (id=1)
            make_message_with_id_parent(1, "mcp", "tools/call", "catenary", None, None),
            // Child 1 (parent_id=1)
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(1),
            ),
            // Child 2 (parent_id=1)
            make_message_with_id_parent(3, "lsp", "workspace/symbol", "taplo", None, Some(1)),
            // Root interruption (no parent_id)
            make_message_with_id_parent(4, "lsp", "$/progress", "rust-analyzer", None, None),
            // Child 3 (parent_id=1)
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
        // First segment, root interruption, Last segment.
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
        // Both segments share the same Rc parent.
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
        // Three segments: First, Middle, Last.
        let messages = vec![
            make_message_with_id_parent(1, "mcp", "tools/call", "catenary", None, None),
            // Segment 1 child
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(1),
            ),
            // Interruption 1
            make_message_with_id_parent(3, "lsp", "$/progress", "rust-analyzer", None, None),
            // Segment 2 child
            make_message_with_id_parent(
                4,
                "lsp",
                "textDocument/references",
                "rust-analyzer",
                None,
                Some(1),
            ),
            // Interruption 2
            make_message_with_id_parent(5, "lsp", "$/progress", "rust-analyzer", None, None),
            // Segment 3 child
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
        // First, interruption, Middle, interruption, Last
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
        // Contiguous children → single Only segment (regression guard).
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
    fn test_segmented_scope_rendering() {
        // Verify ellipsis convention: First → "grep…", Middle → "…grep…",
        // Last → "…grep (metrics)".
        let theme = test_theme();
        let icons = test_icons();
        let make_tool_call = || {
            let mut m = make_message_with_id_parent(1, "mcp", "tools/call", "catenary", None, None);
            m.payload = serde_json::json!({"params": {"name": "grep"}});
            m
        };

        let parent_entry = DisplayEntry::Single {
            index: 0,
            parent_id: None,
        };
        let parent_rc = Rc::new(parent_entry);
        let messages = vec![make_tool_call()];

        // First segment: "grep…"
        let line = format_scope_styled(
            &parent_rc,
            3,
            SegmentPosition::First,
            &messages,
            &icons,
            &theme,
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("grep\u{2026}"),
            "First should render grep…: {text}"
        );
        assert!(
            !text.contains("grep\u{2026}\u{2026}"),
            "First should not have double ellipsis: {text}"
        );

        // Middle segment: "…grep…"
        let line = format_scope_styled(
            &parent_rc,
            2,
            SegmentPosition::Middle,
            &messages,
            &icons,
            &theme,
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\u{2026}grep\u{2026}"),
            "Middle should render …grep…: {text}"
        );

        // Last segment: "…grep" with metrics
        let line = format_scope_styled(
            &parent_rc,
            1,
            SegmentPosition::Last,
            &messages,
            &icons,
            &theme,
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("\u{2026}grep"),
            "Last should render …grep: {text}"
        );

        // Only segment: "grep" without ellipsis
        let line = format_scope_styled(
            &parent_rc,
            5,
            SegmentPosition::Only,
            &messages,
            &icons,
            &theme,
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("grep"), "Only should contain grep: {text}");
        assert!(
            !text.contains('\u{2026}'),
            "Only should not contain ellipsis: {text}"
        );
    }

    #[test]
    fn test_segmented_scope_independent_expansion() {
        // Expand segment 1, verify segment 2 remains collapsed.
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            make_message_with_id_parent(1, "mcp", "tools/call", "catenary", None, None),
            // Segment 1 children
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(1),
            ),
            make_message_with_id_parent(3, "lsp", "workspace/symbol", "taplo", None, Some(1)),
            // Root interruption
            make_message_with_id_parent(4, "lsp", "$/progress", "rust-analyzer", None, None),
            // Segment 2 child
            make_message_with_id_parent(
                5,
                "lsp",
                "textDocument/references",
                "rust-analyzer",
                None,
                Some(1),
            ),
        ];
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(messages);

        // Expand segment 1 only (first child index = 1).
        panel.expanded.insert(1);
        let flat = panel.flat_lines();

        // Segment 1 ScopeHeader + 2 ScopeChildren + interruption + Segment 2 ScopeHeader (collapsed)
        assert_eq!(
            flat.len(),
            5,
            "segment 1 expanded, segment 2 collapsed: {flat:?}"
        );
        assert!(
            matches!(flat[0], FlatLine::ScopeHeader { .. }),
            "first should be segment 1 ScopeHeader"
        );
        assert!(
            matches!(flat[1], FlatLine::ScopeChild { .. }),
            "second should be ScopeChild"
        );
        assert!(
            matches!(flat[2], FlatLine::ScopeChild { .. }),
            "third should be ScopeChild"
        );
        // flat[3] is the interruption (single or collapsed)
        assert!(
            matches!(flat[4], FlatLine::ScopeHeader { .. }),
            "fifth should be segment 2 ScopeHeader (collapsed)"
        );
    }

    #[test]
    fn test_segmented_scope_filter_hides_interruption() {
        // Filter out the interrupting entry. The pipeline runs scope
        // collapse before filtering, so the two segments remain — but
        // the interruption is hidden from the flat line output.
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            {
                let mut m =
                    make_message_with_id_parent(1, "mcp", "tools/call", "catenary", None, None);
                m.payload = serde_json::json!({"params": {"name": "grep"}});
                m
            },
            // Segment 1 child
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(1),
            ),
            // Root interruption — progress with a distinct method for filtering
            make_message_with_id_parent(3, "lsp", "$/progress", "rust-analyzer", None, None),
            // Segment 2 child
            make_message_with_id_parent(
                4,
                "lsp",
                "textDocument/references",
                "rust-analyzer",
                None,
                Some(1),
            ),
        ];
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(messages);

        // Without filter: 2 segments + 1 interruption = 3 flat lines.
        let flat = panel.flat_lines();
        assert_eq!(
            flat.len(),
            3,
            "unfiltered: 2 segments + 1 interruption: {flat:?}"
        );

        // Filter to only show "grep" — matches scope headers but not the
        // progress interruption. Segments remain separate (scope collapse
        // runs before filtering) but the interruption is hidden.
        panel.filter_pattern = Some("grep".to_string());
        let flat = panel.flat_lines();
        assert_eq!(
            flat.len(),
            2,
            "filtered: 2 segments, interruption hidden: {flat:?}"
        );
    }

    #[test]
    fn test_segmented_scope_plain_format() {
        // Verify plain text output includes the ellipsis convention.
        let make_tool_call = || {
            let mut m = make_message_with_id_parent(1, "mcp", "tools/call", "catenary", None, None);
            m.payload = serde_json::json!({"params": {"name": "grep"}});
            m
        };

        let parent_entry = DisplayEntry::Single {
            index: 0,
            parent_id: None,
        };
        let messages = vec![make_tool_call()];

        let plain_first = format_scope_plain(&parent_entry, 3, SegmentPosition::First, &messages);
        assert!(
            plain_first.contains("grep\u{2026}"),
            "First plain should contain grep…: {plain_first}"
        );

        let plain_middle = format_scope_plain(&parent_entry, 2, SegmentPosition::Middle, &messages);
        assert!(
            plain_middle.contains("\u{2026}grep\u{2026}"),
            "Middle plain should contain …grep…: {plain_middle}"
        );

        let plain_last = format_scope_plain(&parent_entry, 1, SegmentPosition::Last, &messages);
        assert!(
            plain_last.contains("\u{2026}grep"),
            "Last plain should contain …grep: {plain_last}"
        );

        let plain_only = format_scope_plain(&parent_entry, 5, SegmentPosition::Only, &messages);
        assert!(
            !plain_only.contains('\u{2026}'),
            "Only plain should not contain ellipsis: {plain_only}"
        );
    }
}

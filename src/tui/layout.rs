// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! BSP layout engine and border junction character computation.
//!
//! Pure geometry — no terminal I/O or rendering framework coupling.
//! Divides a [`Rect`] into [`PanelRect`]s according to a [`Composition`],
//! with optional pin-driven sizing. Computes Unicode box-drawing junction
//! characters for shared borders with focused/unfocused heavy/light distinction.

use std::collections::HashSet;
use std::hash::BuildHasher;

use ratatui::layout::Rect;

// ── Types ───────────────────────────────────────────────────────────────

/// An ordered partition of N panels into rows.
///
/// Each element is the number of panels in that row.
/// Example: `[2, 1]` means 2 panels in row 0, 1 panel in row 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composition(pub Vec<usize>);

impl Composition {
    /// Total number of panels across all rows.
    #[must_use]
    pub fn total(&self) -> usize {
        self.0.iter().sum()
    }
}

/// A positioned panel within the grid.
#[derive(Debug, Clone)]
pub struct PanelRect {
    /// The screen area for this panel.
    pub rect: Rect,
    /// Which row this panel is in (0-indexed).
    pub row: usize,
    /// Which column within the row (0-indexed).
    pub col: usize,
    /// Global panel index (0-indexed, reading order).
    pub index: usize,
}

/// The computed layout of all panels.
#[derive(Debug)]
pub struct PanelLayout {
    /// Positioned panels in reading order.
    pub panels: Vec<PanelRect>,
}

// ── Layout computation ──────────────────────────────────────────────────

/// Compute the layout of panels within the given area.
///
/// Adjacent panels share their border cells (rects overlap by 1 on shared
/// edges). This produces correct junction characters at shared borders.
/// Pinned panels receive proportionally more space according to `pin_ratio`.
/// Remainder pixels are distributed left-to-right, top-to-bottom.
#[must_use]
pub fn compute_layout<S: BuildHasher>(
    area: Rect,
    composition: &Composition,
    pinned: &HashSet<usize, S>,
    pin_ratio: f64,
) -> PanelLayout {
    let num_rows = composition.0.len();
    if num_rows == 0 || area.height == 0 || area.width == 0 {
        return PanelLayout { panels: Vec::new() };
    }

    // Determine which rows contain a pinned panel.
    let mut global_idx = 0usize;
    let row_has_pin: Vec<bool> = composition
        .0
        .iter()
        .map(|&cols| {
            let has = (global_idx..global_idx + cols).any(|i| pinned.contains(&i));
            global_idx += cols;
            has
        })
        .collect();

    // Compute row fence-post gaps (distribute H-1 among num_rows segments).
    let row_weights: Vec<f64> = row_has_pin
        .iter()
        .map(|&has_pin| if has_pin { pin_ratio } else { 1.0 })
        .collect();
    let row_gaps = distribute_weighted(area.height.saturating_sub(1), &row_weights);

    // Build panels row by row.
    let mut panels = Vec::with_capacity(composition.total());
    let mut y_fence = area.y;
    let mut panel_index = 0usize;

    for (row_idx, &cols_in_row) in composition.0.iter().enumerate() {
        let row_gap = row_gaps[row_idx];
        let row_height = row_gap + 1;

        // Column weights for this row.
        let col_weights: Vec<f64> = (0..cols_in_row)
            .map(|c| {
                if pinned.contains(&(panel_index + c)) {
                    pin_ratio
                } else {
                    1.0
                }
            })
            .collect();

        // Distribute W-1 among cols_in_row segments.
        let col_gaps = distribute_weighted(area.width.saturating_sub(1), &col_weights);

        let mut x_fence = area.x;
        for (col_idx, &col_gap) in col_gaps.iter().enumerate() {
            let col_width = col_gap + 1;

            panels.push(PanelRect {
                rect: Rect::new(x_fence, y_fence, col_width, row_height),
                row: row_idx,
                col: col_idx,
                index: panel_index,
            });

            // Next panel starts at the shared border (overlap by 1).
            x_fence += col_gap;
            panel_index += 1;
        }

        // Next row starts at the shared border (overlap by 1).
        y_fence += row_gap;
    }

    PanelLayout { panels }
}

/// Distribute `total` pixels among `n` slots with the given weights.
///
/// Each slot gets at least `floor(total * weight / sum_weights)` pixels.
/// Remainder pixels are distributed to slots in order (left-to-right or
/// top-to-bottom) by largest fractional part, breaking ties by index.
fn distribute_weighted(total: u16, weights: &[f64]) -> Vec<u16> {
    let n = weights.len();
    if n == 0 {
        return Vec::new();
    }

    let sum: f64 = weights.iter().sum();
    if sum <= 0.0 {
        // All zero weights — distribute equally.
        return distribute_weighted(total, &vec![1.0; n]);
    }

    let mut sizes = Vec::with_capacity(n);
    let mut remainders = Vec::with_capacity(n);
    let mut allocated = 0u16;

    for (i, &w) in weights.iter().enumerate() {
        let exact = f64::from(total) * w / sum;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "exact is non-negative and bounded by u16::MAX"
        )]
        let floor = exact.floor() as u16;
        sizes.push(floor);
        remainders.push((exact - f64::from(floor), i));
        allocated += floor;
    }

    // Distribute leftover pixels by largest fractional remainder.
    let mut leftover = total - allocated;
    remainders.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    for &(_, idx) in &remainders {
        if leftover == 0 {
            break;
        }
        sizes[idx] += 1;
        leftover -= 1;
    }

    sizes
}

/// Integer square root rounded up (pure integer math).
const fn isqrt_ceil(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let s = n.isqrt();
    if s * s == n { s } else { s + 1 }
}

// ── Curated layouts ─────────────────────────────────────────────────────

/// Return a curated subset of practical compositions for `n` panels.
///
/// The `w` key cycles through this list.
#[must_use]
pub fn curated_layouts(n: usize) -> Vec<Composition> {
    match n {
        0 => vec![],
        1 => vec![Composition(vec![1])],
        2 => vec![Composition(vec![1, 1]), Composition(vec![2])],
        3 => vec![
            Composition(vec![1, 1, 1]),
            Composition(vec![3]),
            Composition(vec![2, 1]),
            Composition(vec![1, 2]),
        ],
        4 => vec![
            Composition(vec![2, 2]),
            Composition(vec![1, 1, 1, 1]),
            Composition(vec![4]),
            Composition(vec![2, 1, 1]),
            Composition(vec![1, 1, 2]),
            Composition(vec![3, 1]),
            Composition(vec![1, 3]),
        ],
        _ => {
            let mut layouts = Vec::new();
            // Balanced grid: rows of isqrt(n), last row gets remainder.
            let cols = isqrt_ceil(n);
            let full_rows = n / cols;
            let remainder = n % cols;
            let mut balanced = vec![cols; full_rows];
            if remainder > 0 {
                balanced.push(remainder);
            }
            layouts.push(Composition(balanced));
            // Single column.
            layouts.push(Composition(vec![1; n]));
            // Single row.
            layouts.push(Composition(vec![n]));
            // Top-heavy: (ceil(n/2), floor(n/2)).
            let top = n.div_ceil(2);
            let bottom = n - top;
            if bottom > 0 {
                layouts.push(Composition(vec![top, bottom]));
            }
            // Bottom-heavy: (floor(n/2), ceil(n/2)).
            let top2 = n / 2;
            let bottom2 = n - top2;
            if top2 > 0 && vec![top2, bottom2] != vec![top, bottom] {
                layouts.push(Composition(vec![top2, bottom2]));
            }
            layouts
        }
    }
}

// ── Closest composition ─────────────────────────────────────────────────

/// When a panel is opened or closed, pick the best composition for `new_n`
/// that preserves the shape of `old`.
#[must_use]
pub fn closest_composition(old: &Composition, new_n: usize) -> Composition {
    if new_n == 0 {
        return Composition(vec![]);
    }

    let old_total = old.total();
    if new_n == old_total {
        return old.clone();
    }

    let mut rows = old.0.clone();

    if new_n < old_total {
        // Remove panels from the last row first.
        let mut to_remove = old_total - new_n;
        while to_remove > 0 {
            let Some(last) = rows.last_mut() else {
                break;
            };
            if *last <= to_remove {
                to_remove -= *last;
                rows.pop();
            } else {
                *last -= to_remove;
                to_remove = 0;
            }
        }
        if rows.is_empty() {
            return curated_layouts(new_n)
                .into_iter()
                .next()
                .unwrap_or_else(|| Composition(vec![new_n]));
        }
        return Composition(rows);
    }

    // new_n > old_total: add to the last row or append a new row.
    let to_add = new_n - old_total;
    // Heuristic: if the last row would become very wide, start a new row.
    let max_row = rows.iter().copied().max().unwrap_or(1);
    if let Some(last) = rows.last_mut()
        && *last + to_add <= max_row + 1
    {
        *last += to_add;
        return Composition(rows);
    }
    rows.push(to_add);
    Composition(rows)
}

// ── Junction characters ─────────────────────────────────────────────────

/// Bit flags for junction arm presence and weight.
const ARM_UP: u8 = 0b0000_0001;
const ARM_DOWN: u8 = 0b0000_0010;
const ARM_LEFT: u8 = 0b0000_0100;
const ARM_RIGHT: u8 = 0b0000_1000;
const HEAVY_UP: u8 = 0b0001_0000;
const HEAVY_DOWN: u8 = 0b0010_0000;
const HEAVY_LEFT: u8 = 0b0100_0000;
const HEAVY_RIGHT: u8 = 0b1000_0000;

/// At a given screen coordinate where borders meet, determine the correct
/// Unicode box-drawing character.
///
/// Arms belonging to the focused panel are heavy; all others are light.
/// Returns a space if no border arms are present at `(x, y)`.
#[must_use]
pub fn junction_char(focused: Option<usize>, panels: &[PanelRect], x: u16, y: u16) -> char {
    let flags = compute_junction_flags(focused, panels, x, y);
    let arms = flags & 0x0F;
    if arms == 0 {
        return ' ';
    }
    lookup_box_char(flags)
}

/// Compute the junction bitfield for a point.
fn compute_junction_flags(focused: Option<usize>, panels: &[PanelRect], x: u16, y: u16) -> u8 {
    let mut flags = 0u8;

    for panel in panels {
        let r = &panel.rect;
        let left = r.x;
        let right = r.x + r.width.saturating_sub(1);
        let top = r.y;
        let bottom = r.y + r.height.saturating_sub(1);
        let is_focused = focused == Some(panel.index);

        // Check if this panel contributes any arm at (x, y).
        // A panel's border is on its edges.

        // Top edge: horizontal segment from left..=right at y==top.
        // Bottom edge: horizontal segment from left..=right at y==bottom.
        // Left edge: vertical segment from top..=bottom at x==left.
        // Right edge: vertical segment from top..=bottom at x==right.

        // Up arm: panel's top edge is at y, and x is on that edge, and there's
        // a vertical border going up (i.e., x is left or right of the panel).
        // Actually, let's think about this differently.
        //
        // At point (x,y), an "up" arm exists if some panel has a vertical border
        // segment from (x, y-1) to (x, y). That means x == left or x == right,
        // and y-1 >= top and y <= bottom (the point is on the border).
        //
        // Similarly for down, left, right arms.

        // Vertical borders (left and right edges of the panel).
        if x == left && y >= top && y <= bottom {
            // This panel has its left edge at x.
            // Up arm: the segment from (x, y-1) to (x, y) exists if y > top.
            if y > top {
                flags |= ARM_UP;
                if is_focused {
                    flags |= HEAVY_UP;
                }
            }
            // Down arm: segment from (x, y) to (x, y+1) exists if y < bottom.
            if y < bottom {
                flags |= ARM_DOWN;
                if is_focused {
                    flags |= HEAVY_DOWN;
                }
            }
        }
        if x == right && y >= top && y <= bottom {
            // Right edge of this panel.
            if y > top {
                flags |= ARM_UP;
                if is_focused {
                    flags |= HEAVY_UP;
                }
            }
            if y < bottom {
                flags |= ARM_DOWN;
                if is_focused {
                    flags |= HEAVY_DOWN;
                }
            }
        }

        // Horizontal borders (top and bottom edges of the panel).
        if y == top && x >= left && x <= right {
            if x > left {
                flags |= ARM_LEFT;
                if is_focused {
                    flags |= HEAVY_LEFT;
                }
            }
            if x < right {
                flags |= ARM_RIGHT;
                if is_focused {
                    flags |= HEAVY_RIGHT;
                }
            }
        }
        if y == bottom && x >= left && x <= right {
            if x > left {
                flags |= ARM_LEFT;
                if is_focused {
                    flags |= HEAVY_LEFT;
                }
            }
            if x < right {
                flags |= ARM_RIGHT;
                if is_focused {
                    flags |= HEAVY_RIGHT;
                }
            }
        }
    }

    flags
}

/// Map junction flags to a Unicode box-drawing character.
#[allow(
    clippy::too_many_lines,
    reason = "exhaustive match on junction flag combinations"
)]
const fn lookup_box_char(flags: u8) -> char {
    let arms = flags & 0x0F;
    let heavy = flags >> 4;

    // For each arm that is present, check if it's heavy.
    let hu = arms & ARM_UP != 0 && heavy & 0x01 != 0;
    let hd = arms & ARM_DOWN != 0 && heavy & 0x02 != 0;
    let hl = arms & ARM_LEFT != 0 && heavy & 0x04 != 0;
    let hr = arms & ARM_RIGHT != 0 && heavy & 0x08 != 0;

    let up = arms & ARM_UP != 0;
    let down = arms & ARM_DOWN != 0;
    let left = arms & ARM_LEFT != 0;
    let right = arms & ARM_RIGHT != 0;

    match (up, down, left, right) {
        // Single arms (straight segments).
        (true, true, false, false) => {
            if hu || hd {
                '┃'
            } else {
                '│'
            }
        }
        (false, false, true, true) => {
            if hl || hr {
                '━'
            } else {
                '─'
            }
        }

        // Corners.
        (false, true, false, true) => match (hd, hr) {
            (false, false) => '┌',
            (true, true) => '┏',
            (true, false) => '┎',
            (false, true) => '┍',
        },
        (false, true, true, false) => match (hd, hl) {
            (false, false) => '┐',
            (true, true) => '┓',
            (true, false) => '┒',
            (false, true) => '┑',
        },
        (true, false, false, true) => match (hu, hr) {
            (false, false) => '└',
            (true, true) => '┗',
            (true, false) => '┖',
            (false, true) => '┕',
        },
        (true, false, true, false) => match (hu, hl) {
            (false, false) => '┘',
            (true, true) => '┛',
            (true, false) => '┚',
            (false, true) => '┙',
        },

        // T-junctions.
        (true, true, false, true) => {
            // ├ variants
            match (hu, hd, hr) {
                (false, false, false) => '├',
                (true, true, true) => '┣',
                (true, true, false) => '┠',
                (false, false, true) => '┝',
                (true, false, false) => '┞',
                (false, true, false) => '┟',
                (true, false, true) => '┡',
                (false, true, true) => '┢',
            }
        }
        (true, true, true, false) => {
            // ┤ variants
            match (hu, hd, hl) {
                (false, false, false) => '┤',
                (true, true, true) => '┫',
                (true, true, false) => '┨',
                (false, false, true) => '┥',
                (true, false, false) => '┦',
                (false, true, false) => '┧',
                (true, false, true) => '┩',
                (false, true, true) => '┪',
            }
        }
        (false, true, true, true) => {
            // ┬ variants
            match (hd, hl, hr) {
                (false, false, false) => '┬',
                (true, true, true) => '┳',
                (true, false, false) => '┰',
                (false, true, true) => '┯',
                (false, true, false) => '┭',
                (false, false, true) => '┮',
                (true, true, false) => '┱',
                (true, false, true) => '┲',
            }
        }
        (true, false, true, true) => {
            // ┴ variants
            match (hu, hl, hr) {
                (false, false, false) => '┴',
                (true, true, true) => '┻',
                (true, false, false) => '┸',
                (false, true, true) => '┷',
                (false, true, false) => '┵',
                (false, false, true) => '┶',
                (true, true, false) => '┹',
                (true, false, true) => '┺',
            }
        }

        // Cross.
        (true, true, true, true) => {
            match (hu, hd, hl, hr) {
                (false, false, false, false) => '┼',
                (true, true, true, true) => '╋',
                // Heavy vertical, light horizontal.
                (true, true, false, false) => '╂',
                // Light vertical, heavy horizontal.
                (false, false, true, true) => '┿',
                // Heavy up, light rest.
                (true, false, false, false) => '╀',
                // Heavy down, light rest.
                (false, true, false, false) => '╁',
                // Heavy left, light rest.
                (false, false, true, false) => '┽',
                // Heavy right, light rest.
                (false, false, false, true) => '┾',
                // Heavy up+left.
                (true, false, true, false) => '╃',
                // Heavy up+right.
                (true, false, false, true) => '╄',
                // Heavy down+left.
                (false, true, true, false) => '╅',
                // Heavy down+right.
                (false, true, false, true) => '╆',
                // Heavy up+left+right (heavy up, heavy horizontal).
                (true, false, true, true) => '╇',
                // Heavy down+left+right.
                (false, true, true, true) => '╈',
                // Heavy up+down+left.
                (true, true, true, false) => '╉',
                // Heavy up+down+right.
                (true, true, false, true) => '╊',
            }
        }

        // Single arm or no arms — shouldn't normally happen in a grid.
        (true, false, false, false) => {
            if hu {
                '╹'
            } else {
                '╵'
            }
        }
        (false, true, false, false) => {
            if hd {
                '╻'
            } else {
                '╷'
            }
        }
        (false, false, true, false) => {
            if hl {
                '╸'
            } else {
                '╴'
            }
        }
        (false, false, false, true) => {
            if hr {
                '╺'
            } else {
                '╶'
            }
        }

        (false, false, false, false) => ' ',
    }
}

// ── Hit testing ─────────────────────────────────────────────────────────

/// Which panel contains the given screen coordinate?
///
/// Returns the panel index or `None` if the point is on a border or outside.
#[must_use]
pub fn panel_at(layout: &PanelLayout, x: u16, y: u16) -> Option<usize> {
    for panel in &layout.panels {
        let r = &panel.rect;
        // Interior: strictly inside the border (not on the edge).
        if x > r.x
            && x < r.x + r.width.saturating_sub(1)
            && y > r.y
            && y < r.y + r.height.saturating_sub(1)
        {
            return Some(panel.index);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert that panels in a row cover the full width with no gaps.
    /// Adjacent panels overlap by exactly 1 (shared border).
    fn assert_row_coverage(panels: &[&PanelRect], area: Rect) {
        assert_eq!(panels[0].rect.x, area.x, "first panel starts at area.x");
        if let Some(last) = panels.last() {
            assert_eq!(
                last.rect.x + last.rect.width,
                area.x + area.width,
                "last panel ends at area edge"
            );
        }
        for w in panels.windows(2) {
            let right_edge = w[0].rect.x + w[0].rect.width - 1;
            assert_eq!(
                w[1].rect.x, right_edge,
                "adjacent panels share border column"
            );
        }
    }

    #[test]
    fn test_single_panel_fills_area() {
        let area = Rect::new(0, 0, 80, 24);
        let comp = Composition(vec![1]);
        let layout = compute_layout(area, &comp, &HashSet::new(), 1.0);
        assert_eq!(layout.panels.len(), 1);
        assert_eq!(layout.panels[0].rect, area);
    }

    #[test]
    fn test_two_panels_stacked() {
        let area = Rect::new(0, 0, 80, 24);
        let comp = Composition(vec![1, 1]);
        let layout = compute_layout(area, &comp, &HashSet::new(), 1.0);
        assert_eq!(layout.panels.len(), 2);
        // Both full width.
        assert_eq!(layout.panels[0].rect.width, 80);
        assert_eq!(layout.panels[1].rect.width, 80);
        // Stacked vertically with shared border (overlap by 1).
        assert_eq!(layout.panels[0].rect.y, 0);
        let shared_y = layout.panels[0].rect.y + layout.panels[0].rect.height - 1;
        assert_eq!(layout.panels[1].rect.y, shared_y, "rows share border row");
        // Together they cover the full height.
        assert_eq!(layout.panels[1].rect.y + layout.panels[1].rect.height, 24);
    }

    #[test]
    fn test_two_panels_side_by_side() {
        let area = Rect::new(0, 0, 80, 24);
        let comp = Composition(vec![2]);
        let layout = compute_layout(area, &comp, &HashSet::new(), 1.0);
        assert_eq!(layout.panels.len(), 2);
        // Both full height.
        assert_eq!(layout.panels[0].rect.height, 24);
        assert_eq!(layout.panels[1].rect.height, 24);
        // Side by side with shared border.
        assert_eq!(layout.panels[0].rect.x, 0);
        let shared_x = layout.panels[0].rect.x + layout.panels[0].rect.width - 1;
        assert_eq!(layout.panels[1].rect.x, shared_x, "panels share border col");
        // Together they cover the full width.
        assert_eq!(layout.panels[1].rect.x + layout.panels[1].rect.width, 80);
    }

    #[test]
    fn test_three_panels_2_1() {
        let area = Rect::new(0, 0, 80, 24);
        let comp = Composition(vec![2, 1]);
        let layout = compute_layout(area, &comp, &HashSet::new(), 1.0);
        assert_eq!(layout.panels.len(), 3);
        // Top row: two panels sharing a border.
        let top: Vec<&PanelRect> = layout.panels.iter().filter(|p| p.row == 0).collect();
        assert_row_coverage(&top, area);
        // Bottom row: one panel full width.
        assert_eq!(layout.panels[2].rect.width, 80);
        assert_eq!(layout.panels[2].rect.x, 0);
        // Rows share a border.
        let shared_y = layout.panels[0].rect.y + layout.panels[0].rect.height - 1;
        assert_eq!(layout.panels[2].rect.y, shared_y);
        assert_eq!(layout.panels[2].rect.y + layout.panels[2].rect.height, 24);
    }

    #[test]
    fn test_pin_scaling() {
        let area = Rect::new(0, 0, 80, 24);
        let comp = Composition(vec![2]);
        let mut pinned = HashSet::new();
        pinned.insert(0);
        let layout = compute_layout(area, &comp, &pinned, 2.0);
        assert_eq!(layout.panels.len(), 2);
        // Panel 0 (pinned) should be wider than panel 1.
        assert!(
            layout.panels[0].rect.width > layout.panels[1].rect.width,
            "pinned panel 0 ({}) should be wider than unpinned panel 1 ({})",
            layout.panels[0].rect.width,
            layout.panels[1].rect.width,
        );
        // Panels cover the full width (shared border).
        let row: Vec<&PanelRect> = layout.panels.iter().collect();
        assert_row_coverage(&row, area);
    }

    #[test]
    fn test_no_pixel_loss() {
        let area = Rect::new(0, 0, 81, 25);

        // Single-row: 3 panels side by side.
        let comp = Composition(vec![3]);
        let layout = compute_layout(area, &comp, &HashSet::new(), 1.0);
        let row: Vec<&PanelRect> = layout.panels.iter().collect();
        assert_row_coverage(&row, area);
        for p in &layout.panels {
            assert_eq!(p.rect.height, 25, "single-row panels are full height");
        }

        // Single-column: 3 panels stacked.
        let comp2 = Composition(vec![1, 1, 1]);
        let layout2 = compute_layout(area, &comp2, &HashSet::new(), 1.0);
        // First panel starts at top, last panel ends at bottom.
        assert_eq!(layout2.panels[0].rect.y, 0);
        assert_eq!(
            layout2.panels[2].rect.y + layout2.panels[2].rect.height,
            25,
            "panels cover full height"
        );
        // Adjacent rows share borders.
        for w in layout2.panels.windows(2) {
            let shared = w[0].rect.y + w[0].rect.height - 1;
            assert_eq!(w[1].rect.y, shared, "rows share border row");
        }
        for p in &layout2.panels {
            assert_eq!(p.rect.width, 81, "single-column panels are full width");
        }
    }

    #[test]
    fn test_curated_layouts_n1() {
        let layouts = curated_layouts(1);
        assert_eq!(layouts, vec![Composition(vec![1])]);
    }

    #[test]
    fn test_curated_layouts_n4() {
        let layouts = curated_layouts(4);
        assert!(
            layouts.contains(&Composition(vec![2, 2])),
            "N=4 curated layouts should contain (2,2)"
        );
    }

    #[test]
    fn test_closest_composition_add() {
        let old = Composition(vec![2, 1]);
        let result = closest_composition(&old, 4);
        assert_eq!(result.total(), 4);
        // Should add to last row or append a new row.
        assert!(!result.0.is_empty());
    }

    #[test]
    fn test_closest_composition_remove() {
        let old = Composition(vec![2, 2]);
        let result = closest_composition(&old, 3);
        assert_eq!(result.total(), 3);
        // Should remove from last row.
        assert_eq!(result.0, vec![2, 1]);
    }

    #[test]
    fn test_junction_char_all_light() {
        // 2x2 grid in a 9x5 area (enough for shared borders), no focus.
        let area = Rect::new(0, 0, 9, 5);
        let comp = Composition(vec![2, 2]);
        let layout = compute_layout(area, &comp, &HashSet::new(), 1.0);

        // Find the shared border column (panel 0's right edge = panel 1's left edge).
        let shared_x = layout.panels[0].rect.x + layout.panels[0].rect.width - 1;
        // Find the shared border row (panel 0's bottom edge = panel 2's top edge).
        let shared_y = layout.panels[0].rect.y + layout.panels[0].rect.height - 1;

        // Top-middle: T-junction pointing down (┬).
        let ch = junction_char(None, &layout.panels, shared_x, 0);
        assert_eq!(ch, '┬', "top middle should be ┬");

        // Left-middle: T-junction pointing right (├).
        let ch = junction_char(None, &layout.panels, 0, shared_y);
        assert_eq!(ch, '├', "left middle should be ├");

        // Center: cross (┼).
        let ch = junction_char(None, &layout.panels, shared_x, shared_y);
        assert_eq!(ch, '┼', "center should be ┼");
    }

    #[test]
    fn test_junction_char_focused_top_left() {
        // 2x2 grid in a 9x5 area, focus on panel 0 (top-left).
        let area = Rect::new(0, 0, 9, 5);
        let comp = Composition(vec![2, 2]);
        let layout = compute_layout(area, &comp, &HashSet::new(), 1.0);

        let shared_x = layout.panels[0].rect.x + layout.panels[0].rect.width - 1;
        let shared_y = layout.panels[0].rect.y + layout.panels[0].rect.height - 1;

        // At the center junction, panel 0's right edge and bottom edge are heavy.
        // Heavy: up arm (panel 0's right edge) and left arm (panel 0's bottom edge).
        // Light: down arm and right arm (unfocused panels).
        let ch = junction_char(Some(0), &layout.panels, shared_x, shared_y);
        assert_eq!(ch, '╃', "center with focus on panel 0 should be ╃");
    }

    #[test]
    fn test_panel_at_inside() {
        let area = Rect::new(0, 0, 80, 24);
        let comp = Composition(vec![2]);
        let layout = compute_layout(area, &comp, &HashSet::new(), 1.0);
        // Point inside panel 0 (left half).
        assert_eq!(panel_at(&layout, 10, 10), Some(0));
        // Point inside panel 1 (right half, well past shared border).
        assert_eq!(panel_at(&layout, 60, 10), Some(1));
    }

    #[test]
    fn test_panel_at_border() {
        let area = Rect::new(0, 0, 80, 24);
        let comp = Composition(vec![2]);
        let layout = compute_layout(area, &comp, &HashSet::new(), 1.0);
        // Point on the top border.
        assert_eq!(panel_at(&layout, 10, 0), None);
        // Shared border column: both panels claim it as a border.
        let shared_x = layout.panels[0].rect.x + layout.panels[0].rect.width - 1;
        assert_eq!(
            panel_at(&layout, shared_x, 10),
            None,
            "shared border returns None"
        );
        // Outer borders also return None.
        assert_eq!(panel_at(&layout, 0, 10), None, "left outer border");
        assert_eq!(panel_at(&layout, 79, 10), None, "right outer border");
    }
}

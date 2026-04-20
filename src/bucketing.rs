// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Two-stage bucketing for grep tier 3 and glob tier 3 output.
//!
//! Stage 1 ([`bucket_separators`]) groups strings by longest common prefix at
//! separator boundaries (`_`, `-`, `.`, space). Stage 2 ([`bucket_trie`])
//! applies trie-based radix compaction when separator structure is absent.
//!
//! The main entry point [`bucket`] runs stage 1, then optionally falls back to
//! stage 2 when `trie_fallback` is `true` and stage 1 produces only a single
//! catch-all bucket.

use std::collections::BTreeMap;

/// A bucket produced by the bucketing algorithm.
pub struct Bucket {
    /// The prefix pattern (e.g., `"test_mcp_*"`).
    pub pattern: String,
    /// Number of entries in this bucket.
    pub count: usize,
    /// If expanded: entries with detail. If collapsed: `None`.
    pub entries: Option<Vec<BucketEntry>>,
}

/// A single entry within an expanded bucket.
pub struct BucketEntry {
    /// The full string (filename, matched text, etc.).
    pub value: String,
    /// Opaque context carried with this entry.
    pub context: Option<String>,
}

/// Main entry point.
///
/// Runs stage 1 (separator-aware). If `trie_fallback` is `true` and stage 1
/// produces a single catch-all `"*"` bucket, runs stage 2 (trie). Glob calls
/// with `trie_fallback = false`. Grep calls with `trie_fallback = true`.
#[must_use]
pub fn bucket(input: &[BucketEntry], budget: usize, trie_fallback: bool) -> Vec<Bucket> {
    if input.is_empty() {
        return Vec::new();
    }

    let mut buckets = bucket_separators(input, budget);

    // If separator bucketing produced a single catch-all and trie fallback is
    // requested, try the trie.
    if trie_fallback && buckets.len() == 1 && buckets[0].pattern == "*" {
        buckets = bucket_trie(input, budget);
    }

    collapse_to_budget(&mut buckets, budget);
    buckets
}

/// Estimate the rendered character cost of a bucket slice.
#[must_use]
pub fn rendered_size(buckets: &[Bucket]) -> usize {
    buckets.iter().map(bucket_rendered_size).sum()
}

// ---------------------------------------------------------------------------
// Stage 1: separator-aware bucketing
// ---------------------------------------------------------------------------

const SEPARATORS: &[char] = &['_', '-', '.', ' '];

/// Separator-aware bucketing.
///
/// Groups input strings by longest common prefix at separator boundaries,
/// then collapses to fit within `budget`.
#[must_use]
pub fn bucket_separators(input: &[BucketEntry], budget: usize) -> Vec<Bucket> {
    if input.is_empty() {
        return Vec::new();
    }

    // Find separator positions for each value and group by longest common
    // prefix at a separator boundary.
    let groups = group_by_separator_prefix(input);

    // If grouping is degenerate — single group holding everything, or every
    // entry in its own group (no shared separator prefix) — return a single
    // catch-all bucket.
    let has_useful_grouping = groups.values().any(|indices| indices.len() > 1);
    if !has_useful_grouping && input.len() > 1 {
        return vec![make_catch_all(input)];
    }

    let mut buckets = groups_to_buckets(&groups, input);

    // Evenness check: if one bucket holds > 80% of entries and we have more
    // than 2 groups, try a shallower split.
    if buckets.len() > 2 {
        let max_count = buckets.iter().map(|b| b.count).max().unwrap_or(0);
        let threshold = (input.len() * 4) / 5; // 80%
        if max_count > threshold {
            let shallow = group_by_separator_prefix_depth(input, 1);
            if shallow.len() > 1 {
                let shallow_buckets = groups_to_buckets(&shallow, input);
                let shallow_max = shallow_buckets.iter().map(|b| b.count).max().unwrap_or(0);
                if shallow_max < max_count {
                    buckets = shallow_buckets;
                }
            }
        }
    }

    collapse_to_budget(&mut buckets, budget);
    buckets
}

/// Group input entries by the longest common prefix up to a separator boundary.
fn group_by_separator_prefix(input: &[BucketEntry]) -> BTreeMap<String, Vec<usize>> {
    let mut prefix_map: BTreeMap<String, Vec<usize>> = BTreeMap::new();

    for (i, entry) in input.iter().enumerate() {
        let prefix = longest_separator_prefix(&entry.value, input);
        prefix_map.entry(prefix).or_default().push(i);
    }

    prefix_map
}

/// Group by separator prefix at a specific maximum depth (number of separator
/// segments to use).
fn group_by_separator_prefix_depth(
    input: &[BucketEntry],
    max_segments: usize,
) -> BTreeMap<String, Vec<usize>> {
    let mut prefix_map: BTreeMap<String, Vec<usize>> = BTreeMap::new();

    for (i, entry) in input.iter().enumerate() {
        let prefix = prefix_at_depth(&entry.value, max_segments);
        prefix_map.entry(prefix).or_default().push(i);
    }

    prefix_map
}

/// Find the longest prefix of `value` up to a separator boundary that is shared
/// with at least one other entry in `input`.
fn longest_separator_prefix(value: &str, input: &[BucketEntry]) -> String {
    let sep_positions: Vec<usize> = value
        .char_indices()
        .filter(|(_, c)| SEPARATORS.contains(c))
        .map(|(i, _)| i)
        .collect();

    // Try from the deepest separator boundary backward.
    for &pos in sep_positions.iter().rev() {
        let candidate = &value[..=pos];
        let matches = input
            .iter()
            .filter(|e| e.value.starts_with(candidate))
            .count();
        if matches > 1 {
            return candidate.to_owned();
        }
    }

    // No shared separator prefix — this entry stands alone.
    value.to_owned()
}

/// Extract prefix up to the Nth separator boundary.
fn prefix_at_depth(value: &str, max_segments: usize) -> String {
    let mut count = 0;
    for (i, c) in value.char_indices() {
        if SEPARATORS.contains(&c) {
            count += 1;
            if count >= max_segments {
                return value[..=i].to_owned();
            }
        }
    }
    value.to_owned()
}

/// Convert grouped indices into `Bucket` values.
fn groups_to_buckets(groups: &BTreeMap<String, Vec<usize>>, input: &[BucketEntry]) -> Vec<Bucket> {
    let mut buckets = Vec::with_capacity(groups.len());

    for (prefix, indices) in groups {
        if indices.len() == 1 {
            // Single-entry group: show the full string, no wildcard.
            let entry = &input[indices[0]];
            buckets.push(Bucket {
                pattern: entry.value.clone(),
                count: 1,
                entries: Some(vec![BucketEntry {
                    value: entry.value.clone(),
                    context: entry.context.clone(),
                }]),
            });
        } else {
            let pattern = format!("{prefix}*");
            let entries: Vec<BucketEntry> = indices
                .iter()
                .map(|&i| BucketEntry {
                    value: input[i].value.clone(),
                    context: input[i].context.clone(),
                })
                .collect();
            let count = entries.len();
            buckets.push(Bucket {
                pattern,
                count,
                entries: Some(entries),
            });
        }
    }

    buckets
}

fn make_catch_all(input: &[BucketEntry]) -> Bucket {
    let entries: Vec<BucketEntry> = input
        .iter()
        .map(|e| BucketEntry {
            value: e.value.clone(),
            context: e.context.clone(),
        })
        .collect();
    Bucket {
        pattern: "*".to_owned(),
        count: entries.len(),
        entries: Some(entries),
    }
}

// ---------------------------------------------------------------------------
// Stage 2: trie-based radix compaction
// ---------------------------------------------------------------------------

/// Trie node for radix compaction.
struct TrieNode {
    children: BTreeMap<char, Self>,
    count: usize,
    terminal: bool,
}

impl TrieNode {
    const fn new() -> Self {
        Self {
            children: BTreeMap::new(),
            count: 0,
            terminal: false,
        }
    }

    fn insert(&mut self, s: &str) {
        self.count += 1;
        if let Some(c) = s.chars().next() {
            let rest = &s[c.len_utf8()..];
            self.children
                .entry(c)
                .or_insert_with(Self::new)
                .insert(rest);
        } else {
            self.terminal = true;
        }
    }
}

/// Trie-based radix compaction.
///
/// Fallback for strings with no separator structure.
#[must_use]
pub fn bucket_trie(input: &[BucketEntry], budget: usize) -> Vec<Bucket> {
    if input.is_empty() {
        return Vec::new();
    }
    if input.len() == 1 {
        return vec![Bucket {
            pattern: input[0].value.clone(),
            count: 1,
            entries: Some(vec![BucketEntry {
                value: input[0].value.clone(),
                context: input[0].context.clone(),
            }]),
        }];
    }

    let mut root = TrieNode::new();
    for entry in input {
        root.insert(&entry.value);
    }

    // Build initial buckets by walking the trie in BFS order and deciding
    // whether to expand or collapse each node.
    let mut buckets: Vec<(String, usize)> = Vec::new();
    expand_trie_node(&root, String::new(), &mut buckets, budget);

    // Enforce minimum of 2 buckets by progressively expanding deeper.
    let mut depth = 1;
    while buckets.len() < 2 && input.len() >= 2 {
        buckets.clear();
        force_expand_depth(&root, String::new(), &mut buckets, depth);
        // If expanding deeper didn't add any new buckets, the trie is
        // exhausted (all leaves reached). Break to avoid an infinite loop.
        if depth > input.iter().map(|e| e.value.len()).max().unwrap_or(0) {
            break;
        }
        depth += 1;
    }

    // Map back to full Bucket structs with entries.
    let mut result: Vec<Bucket> = Vec::with_capacity(buckets.len());
    for (pattern, count) in &buckets {
        let prefix = pattern.trim_end_matches('*');
        let entries: Vec<BucketEntry> = input
            .iter()
            .filter(|e| e.value.starts_with(prefix))
            .map(|e| BucketEntry {
                value: e.value.clone(),
                context: e.context.clone(),
            })
            .collect();
        result.push(Bucket {
            pattern: pattern.clone(),
            count: *count,
            entries: if entries.is_empty() {
                None
            } else {
                Some(entries)
            },
        });
    }

    collapse_to_budget(&mut result, budget);
    result
}

/// Recursively decide whether to expand or collapse a trie node.
fn expand_trie_node(
    node: &TrieNode,
    prefix: String,
    buckets: &mut Vec<(String, usize)>,
    budget: usize,
) {
    if node.children.is_empty() {
        // Leaf: emit as-is.
        let pattern = if node.terminal {
            prefix
        } else {
            format!("{prefix}*")
        };
        buckets.push((pattern, node.count));
        return;
    }

    // Compute current CV (treating this node as one collapsed bucket alongside
    // existing buckets).
    let current_counts: Vec<usize> = buckets
        .iter()
        .map(|(_, c)| *c)
        .chain(std::iter::once(node.count))
        .collect();

    // Compute hypothetical CV if we expand this node's children.
    let child_counts: Vec<usize> = node
        .children
        .values()
        .map(|c| c.count)
        .chain(if node.terminal { Some(1) } else { None })
        .collect();

    let expanded_counts: Vec<usize> = buckets
        .iter()
        .map(|(_, c)| *c)
        .chain(child_counts.iter().copied())
        .collect();

    // Check budget: each expanded child costs ~20 chars.
    let expanded_cost: usize = child_counts.len() * 20;
    let within_budget = expanded_cost <= budget.saturating_sub(rendered_size_estimate(buckets));

    let cv_improves = cv(&expanded_counts) < cv(&current_counts);

    if within_budget && cv_improves && child_counts.len() > 1 {
        // Expand: recurse into children.
        for (&c, child) in &node.children {
            let mut child_prefix = prefix.clone();
            child_prefix.push(c);
            expand_trie_node(child, child_prefix, buckets, budget);
        }
        if node.terminal {
            buckets.push((prefix, 1));
        }
    } else {
        // Collapse: emit this subtree as one bucket.
        let pattern = if node.count == 1 && node.terminal && node.children.is_empty() {
            prefix
        } else {
            format!("{prefix}*")
        };
        buckets.push((pattern, node.count));
    }
}

/// Force-expand to a given depth to ensure minimum bucket count.
fn force_expand_depth(
    node: &TrieNode,
    prefix: String,
    buckets: &mut Vec<(String, usize)>,
    remaining_depth: usize,
) {
    if remaining_depth == 0 || node.children.is_empty() {
        let pattern = if node.count == 1 && node.terminal && node.children.is_empty() {
            prefix
        } else {
            format!("{prefix}*")
        };
        buckets.push((pattern, node.count));
        return;
    }

    for (&c, child) in &node.children {
        let mut child_prefix = prefix.clone();
        child_prefix.push(c);
        force_expand_depth(child, child_prefix, buckets, remaining_depth - 1);
    }
    if node.terminal {
        buckets.push((prefix, 1));
    }
}

/// Quick estimate of rendered size for trie expansion budget checks.
fn rendered_size_estimate(buckets: &[(String, usize)]) -> usize {
    buckets
        .iter()
        .map(|(p, c)| p.len() + count_digits(*c) + 6)
        .sum()
}

// ---------------------------------------------------------------------------
// Collapse / degrade
// ---------------------------------------------------------------------------

/// When total rendered output exceeds budget, collapse expanded buckets
/// (smallest first) to bare handles until it fits.
fn collapse_to_budget(buckets: &mut [Bucket], budget: usize) {
    // Iteratively collapse the smallest expanded bucket until we fit.
    while rendered_size(buckets) > budget {
        // Find the expanded bucket with the smallest count.
        let smallest = buckets
            .iter()
            .enumerate()
            .filter(|(_, b)| b.entries.is_some() && b.count > 1)
            .min_by_key(|(_, b)| b.count)
            .map(|(i, _)| i);

        if let Some(i) = smallest {
            buckets[i].entries = None;
        } else {
            // All buckets are already bare handles. Nothing more to collapse.
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Rendered size of a single bucket.
fn bucket_rendered_size(b: &Bucket) -> usize {
    match &b.entries {
        None => {
            // Bare handle: "pattern (count)\n"
            b.pattern.len() + count_digits(b.count) + 4
        }
        Some(entries) if b.count == 1 => {
            // Single entry shown as full string.
            entries.first().map_or(0, |e| {
                e.value.len() + e.context.as_ref().map_or(0, |c| c.len() + 2) + 1
            })
        }
        Some(entries) => {
            // Pattern header + expanded entries.
            let header = b.pattern.len() + count_digits(b.count) + 4;
            let body: usize = entries
                .iter()
                .map(|e| {
                    // tab + value + optional context + newline
                    e.value.len() + e.context.as_ref().map_or(0, |c| c.len() + 2) + 2
                })
                .sum();
            header + body
        }
    }
}

/// Number of decimal digits in a `usize`.
const fn count_digits(n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    let mut digits = 0;
    let mut val = n;
    while val > 0 {
        digits += 1;
        val /= 10;
    }
    digits
}

/// Coefficient of variation: stddev / mean. Returns `f64::MAX` for
/// empty or zero-mean input.
fn cv(counts: &[usize]) -> f64 {
    if counts.is_empty() {
        return f64::MAX;
    }
    #[allow(clippy::cast_precision_loss, reason = "bucket counts are small")]
    let n = counts.len() as f64;
    #[allow(clippy::cast_precision_loss, reason = "bucket counts are small")]
    let sum: f64 = counts.iter().map(|&c| c as f64).sum();
    let mean = sum / n;
    if mean == 0.0 {
        return f64::MAX;
    }
    #[allow(clippy::cast_precision_loss, reason = "bucket counts are small")]
    let variance: f64 = counts
        .iter()
        .map(|&c| (c as f64 - mean).powi(2))
        .sum::<f64>()
        / n;
    variance.sqrt() / mean
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to make `BucketEntry` values without context.
    fn entries(values: &[&str]) -> Vec<BucketEntry> {
        values
            .iter()
            .map(|v| BucketEntry {
                value: (*v).to_owned(),
                context: None,
            })
            .collect()
    }

    #[test]
    fn test_separator_basic() {
        let input = entries(&["test_a_1", "test_a_2", "test_b_1", "test_b_2"]);
        let buckets = bucket_separators(&input, 10_000);
        assert!(
            buckets.len() >= 2,
            "expected at least 2 buckets, got {}: {:?}",
            buckets.len(),
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
        let patterns: Vec<&str> = buckets.iter().map(|b| b.pattern.as_str()).collect();
        assert!(
            patterns.iter().any(|p| p.contains("test_a_")),
            "missing test_a_* bucket: {patterns:?}"
        );
        assert!(
            patterns.iter().any(|p| p.contains("test_b_")),
            "missing test_b_* bucket: {patterns:?}"
        );
        for b in &buckets {
            if b.pattern.contains("test_a_") || b.pattern.contains("test_b_") {
                assert_eq!(b.count, 2, "bucket {} should have 2 entries", b.pattern);
            }
        }
    }

    #[test]
    fn test_separator_mixed_delimiters() {
        let input = entries(&[
            "config-dev-a",
            "config-dev-b",
            "config-prod-a",
            "config-prod-b",
            "data_file_1",
            "data_file_2",
        ]);
        let buckets = bucket_separators(&input, 10_000);
        assert!(
            buckets.len() >= 2,
            "expected at least 2 buckets, got {}: {:?}",
            buckets.len(),
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_separator_dot() {
        let input = entries(&["config.dev.json", "config.prod.json", "data.json"]);
        let buckets = bucket_separators(&input, 10_000);
        assert!(
            buckets.len() >= 2,
            "expected at least 2 buckets for dot-separated input, got {}: {:?}",
            buckets.len(),
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
        let has_config = buckets.iter().any(|b| b.pattern.starts_with("config."));
        assert!(
            has_config,
            "expected a config.* bucket: {:?}",
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_separator_evenness() {
        // One group has 90% of entries — should trigger the evenness check.
        let owned: Vec<String> = (0..18)
            .map(|i| format!("test_a_{i}"))
            .chain((0..2).map(|i| format!("test_b_{i}")))
            .collect();
        let input: Vec<BucketEntry> = owned
            .iter()
            .map(|v| BucketEntry {
                value: v.clone(),
                context: None,
            })
            .collect();
        let buckets = bucket_separators(&input, 10_000);
        assert!(
            buckets.len() >= 2,
            "expected evenness check to produce at least 2 buckets, got {}: {:?}",
            buckets.len(),
            buckets
                .iter()
                .map(|b| (&b.pattern, b.count))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_trie_basic() {
        let input = entries(&[
            "alpha",
            "alphabeta",
            "alphacat",
            "bravo",
            "bravocat",
            "charlie",
        ]);
        let buckets = bucket_trie(&input, 10_000);
        assert!(
            buckets.len() >= 2,
            "trie should produce at least 2 buckets, got {}: {:?}",
            buckets.len(),
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_trie_cv_improvement() {
        // Two clear prefixes: "aaa*" (3) and "bbb*" (3) — perfectly even.
        let input = entries(&["aaa1", "aaa2", "aaa3", "bbb1", "bbb2", "bbb3"]);
        let buckets = bucket_trie(&input, 10_000);
        assert!(
            buckets.len() >= 2,
            "splitting should happen when CV improves, got {}: {:?}",
            buckets.len(),
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_trie_cv_no_improvement() {
        // One huge cluster, one tiny — CV may not improve on expansion.
        let mut values: Vec<String> = (0..50).map(|i| format!("a{i:03}")).collect();
        values.push("b1".to_owned());
        let input: Vec<BucketEntry> = values
            .iter()
            .map(|v| BucketEntry {
                value: v.clone(),
                context: None,
            })
            .collect();
        let buckets = bucket_trie(&input, 10_000);
        assert!(
            buckets.len() >= 2,
            "expected at least 2 buckets, got {}: {:?}",
            buckets.len(),
            buckets
                .iter()
                .map(|b| (&b.pattern, b.count))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_budget_collapse() {
        // 100 strings, small budget. All multi-entry buckets should be bare
        // handles.
        let owned: Vec<String> = (0..100).map(|i| format!("item_{i}")).collect();
        let input: Vec<BucketEntry> = owned
            .iter()
            .map(|v| BucketEntry {
                value: v.clone(),
                context: None,
            })
            .collect();
        let buckets = bucket(&input, 50, false);
        for b in &buckets {
            if b.count > 1 {
                assert!(
                    b.entries.is_none(),
                    "bucket {} ({}) should be a bare handle at budget=50",
                    b.pattern,
                    b.count
                );
            }
        }
    }

    #[test]
    fn test_budget_expand() {
        let input = entries(&["alpha", "beta", "gamma"]);
        let buckets = bucket(&input, 10_000, true);
        let has_expanded = buckets.iter().any(|b| b.entries.is_some());
        assert!(
            has_expanded,
            "with large budget, some buckets should be expanded"
        );
    }

    #[test]
    fn test_minimum_two_buckets() {
        // Adversarial: all strings same prefix, no separators.
        let owned: Vec<String> = (0..10).map(|i| format!("x{i}")).collect();
        let input: Vec<BucketEntry> = owned
            .iter()
            .map(|v| BucketEntry {
                value: v.clone(),
                context: None,
            })
            .collect();
        let buckets = bucket_trie(&input, 10_000);
        assert!(
            buckets.len() >= 2,
            "trie must produce at least 2 buckets, got {}: {:?}",
            buckets.len(),
            buckets
                .iter()
                .map(|b| (&b.pattern, b.count))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_no_trie_fallback() {
        let input = entries(&["alpha", "alphabeta", "bravo"]);
        let buckets = bucket(&input, 10_000, false);
        assert_eq!(
            buckets.len(),
            1,
            "with trie_fallback=false and no separators, expected 1 catch-all, got {:?}",
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
        assert_eq!(buckets[0].pattern, "*");
    }

    #[test]
    fn test_trie_fallback() {
        let input = entries(&[
            "alpha",
            "alphabeta",
            "alphacat",
            "bravo",
            "bravocat",
            "charlie",
        ]);
        let buckets = bucket(&input, 10_000, true);
        assert!(
            buckets.len() >= 2,
            "with trie_fallback=true, should produce useful buckets, got {:?}",
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_rendered_size() {
        let buckets = vec![
            Bucket {
                pattern: "test_*".to_owned(),
                count: 5,
                entries: None,
            },
            Bucket {
                pattern: "data.json".to_owned(),
                count: 1,
                entries: Some(vec![BucketEntry {
                    value: "data.json".to_owned(),
                    context: None,
                }]),
            },
        ];
        let size = rendered_size(&buckets);
        assert!(size > 0, "rendered size should be positive");
        assert!(size < 100, "rendered size should be reasonable, got {size}");
    }

    #[test]
    fn test_adversarial_long_prefixes() {
        let long_prefix: String = "a".repeat(1500);
        let owned: Vec<String> = (0..10).map(|i| format!("{long_prefix}{i}")).collect();
        let input: Vec<BucketEntry> = owned
            .iter()
            .map(|v| BucketEntry {
                value: v.clone(),
                context: None,
            })
            .collect();
        let buckets = bucket(&input, 50_000, true);
        assert!(
            buckets.len() >= 2,
            "adversarial long prefixes should produce at least 2 buckets, got {}: {:?}",
            buckets.len(),
            buckets
                .iter()
                .map(|b| (&b.pattern, b.count))
                .collect::<Vec<_>>()
        );
    }
}

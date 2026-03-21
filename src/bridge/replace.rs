// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Replace tool core: input parsing, flag validation, edit application,
//! file scoping, and diff extraction.

use anyhow::{Result, anyhow};
use globset::Glob;
use ignore::WalkBuilder;
use regex::Regex;
use serde::Deserialize;
use similar::{ChangeTag, TextDiff};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use super::diagnostics_server::resolve_path;

/// Input parameters for the `replace` tool.
#[derive(Debug, Deserialize)]
pub struct ReplaceInput {
    /// File path or glob pattern (required).
    pub glob: String,
    /// List of edit operations to apply sequentially.
    pub edits: Vec<EditEntry>,
    /// Line ranges to constrain replacements (e.g., `"1-10 30 70-"`).
    #[serde(default)]
    pub lines: Option<String>,
    /// Glob pattern to exclude from matches.
    #[serde(default)]
    pub exclude: Option<String>,
    /// Include gitignored files in glob expansion.
    #[serde(default)]
    pub include_gitignored: bool,
    /// Include hidden/dot files in glob expansion.
    #[serde(default)]
    pub include_hidden: bool,
}

/// A single edit operation: find `old`, replace with `new`.
#[derive(Debug, Deserialize)]
pub struct EditEntry {
    /// Text to find (literal string or regex pattern).
    pub old: String,
    /// Replacement text (literal or with capture groups in regex mode).
    pub new: String,
    /// Optional flags: `g` (global), `r` (regex), `i`, `m`, `s`.
    #[serde(default)]
    pub flags: Option<String>,
}

/// Parsed flags for a single edit operation.
#[derive(Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "flags map 1:1 to user input characters"
)]
struct EditFlags {
    /// Replace all occurrences (`g`).
    global: bool,
    /// Treat `old` as a regex pattern (`r`, or implied by `i`/`m`/`s`).
    regex: bool,
    /// Case-insensitive matching (`i`, implies regex).
    case_insensitive: bool,
    /// Multiline mode (`m`, implies regex).
    multiline: bool,
    /// Dotall mode (`s`, implies regex).
    dotall: bool,
}

/// Parse a flags string into an [`EditFlags`] struct.
///
/// Valid characters: `g`, `r`, `i`, `m`, `s`. Unknown characters produce
/// an error referencing `edit_index`.
fn parse_flags(flags: Option<&str>, edit_index: usize) -> Result<EditFlags> {
    let mut result = EditFlags {
        global: false,
        regex: false,
        case_insensitive: false,
        multiline: false,
        dotall: false,
    };

    let Some(flags) = flags else {
        return Ok(result);
    };

    for c in flags.chars() {
        match c {
            'g' => result.global = true,
            'r' => result.regex = true,
            'i' => result.case_insensitive = true,
            'm' => result.multiline = true,
            's' => result.dotall = true,
            _ => return Err(anyhow!("unknown flag '{c}' in edit #{edit_index}")),
        }
    }

    if result.case_insensitive || result.multiline || result.dotall {
        result.regex = true;
    }

    Ok(result)
}

/// A validated edit ready for application.
pub enum ParsedEdit {
    /// Literal string replacement.
    Literal {
        /// Text to find.
        old: String,
        /// Replacement text.
        new: String,
        /// Replace all occurrences.
        global: bool,
    },
    /// Regex replacement.
    Regex {
        /// Compiled regex pattern.
        pattern: Regex,
        /// Replacement string (supports `$1`, `$2`, `${name}`).
        replacement: String,
        /// Replace all occurrences.
        global: bool,
    },
}

/// Normalize a regex replacement string for the `regex` crate.
///
/// Translates conventional `\`-escape syntax into the regex crate's
/// `$$`-escape syntax and braces bare numeric references.
///
/// Valid forms:
/// - `$N` (bare digits) → `${N}` (auto-braced)
/// - `${N}` (braced digits) → pass through
/// - `\$` → literal `$` (emitted as `$$`)
/// - `\\` → literal `\`
/// - `\` + other → literal `\` + char (passthrough)
/// - `$` + anything else → error
///
/// Named capture groups (`${name}`) are not supported.
///
/// # Errors
///
/// Returns an error if `$` is followed by a non-digit character,
/// or if `${...}` contains non-digit characters.
fn normalize_replacement(s: &str, edit_index: usize) -> Result<String> {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('$') => {
                    chars.next();
                    result.push_str("$$");
                }
                Some('\\') => {
                    chars.next();
                    result.push('\\');
                }
                _ => {
                    result.push('\\');
                }
            }
        } else if c == '$' {
            match chars.peek() {
                Some('{') => {
                    chars.next();
                    let mut name = String::new();
                    let mut closed = false;
                    for ch in chars.by_ref() {
                        if ch == '}' {
                            closed = true;
                            break;
                        }
                        name.push(ch);
                    }
                    if !closed {
                        return Err(anyhow!(
                            "unclosed ${{}} in replacement for edit #{edit_index}"
                        ));
                    }
                    if name.is_empty() || !name.chars().all(|c| c.is_ascii_digit()) {
                        return Err(anyhow!(
                            "named capture groups are not supported in edit \
                             #{edit_index}, use numbered groups ($1, ${{1}})"
                        ));
                    }
                    result.push_str("${");
                    result.push_str(&name);
                    result.push('}');
                }
                Some(d) if d.is_ascii_digit() => {
                    let mut digits = String::new();
                    while let Some(&d) = chars.peek() {
                        if d.is_ascii_digit() {
                            digits.push(d);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    result.push_str("${");
                    result.push_str(&digits);
                    result.push('}');
                }
                Some(&next) => {
                    return Err(anyhow!(
                        "invalid '$' in replacement for edit #{edit_index}: \
                         expected digit or '{{', got '{next}'. \
                         Use \\$ for a literal dollar sign"
                    ));
                }
                None => {
                    return Err(anyhow!(
                        "trailing '$' in replacement for edit #{edit_index}. \
                         Use \\$ for a literal dollar sign"
                    ));
                }
            }
        } else {
            result.push(c);
        }
    }

    Ok(result)
}

/// Parse a single [`EditEntry`] into a [`ParsedEdit`].
fn parse_edit(entry: &EditEntry, index: usize) -> Result<ParsedEdit> {
    let flags = parse_flags(entry.flags.as_deref(), index)?;

    if flags.regex {
        let mut prefix = String::from("(?");
        if flags.case_insensitive {
            prefix.push('i');
        }
        if flags.multiline {
            prefix.push('m');
        }
        if flags.dotall {
            prefix.push('s');
        }
        prefix.push(')');

        let pattern_str = if prefix == "(?)" {
            entry.old.clone()
        } else {
            format!("{prefix}{}", entry.old)
        };

        let pattern = Regex::new(&pattern_str)
            .map_err(|err| anyhow!("invalid regex in edit #{index}: {err}"))?;

        Ok(ParsedEdit::Regex {
            pattern,
            replacement: normalize_replacement(&entry.new, index)?,
            global: flags.global,
        })
    } else {
        Ok(ParsedEdit::Literal {
            old: entry.old.clone(),
            new: entry.new.clone(),
            global: flags.global,
        })
    }
}

/// Parse and validate all edit entries.
///
/// # Errors
///
/// Returns an error if the edits array is empty, any flag string contains
/// an unknown character, or a regex pattern fails to compile.
pub fn parse_edits(edits: &[EditEntry]) -> Result<Vec<ParsedEdit>> {
    if edits.is_empty() {
        return Err(anyhow!("at least one edit is required"));
    }

    edits
        .iter()
        .enumerate()
        .map(|(i, e)| parse_edit(e, i))
        .collect()
}

/// Parsed line ranges for constraining replacements.
///
/// Internally stored as sorted, merged closed ranges of 1-based line numbers.
pub struct LineRanges(Vec<(usize, usize)>);

impl LineRanges {
    /// Check if a 1-based line number falls within any parsed range.
    #[must_use]
    pub fn contains(&self, line: usize) -> bool {
        self.0
            .iter()
            .any(|&(start, end)| line >= start && line <= end)
    }
}

/// Parse a space-separated line range string into a [`LineRanges`].
///
/// Supports: `N` (single line), `N-M` (closed range), `N-` (open-ended).
/// All line numbers are 1-based. Overlapping or adjacent ranges are merged.
///
/// # Errors
///
/// Returns an error if any token is not a valid line number or range.
pub fn parse_line_ranges(s: &str) -> Result<LineRanges> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();

    for token in s.split_whitespace() {
        if let Some(dash_pos) = token.find('-') {
            if dash_pos == 0 {
                return Err(anyhow!("invalid line range: '{token}'"));
            }

            let start: usize = token[..dash_pos]
                .parse()
                .map_err(|_| anyhow!("invalid line number in range: '{token}'"))?;

            let after_dash = &token[dash_pos + 1..];
            let end = if after_dash.is_empty() {
                usize::MAX
            } else {
                after_dash
                    .parse()
                    .map_err(|_| anyhow!("invalid line number in range: '{token}'"))?
            };

            if start == 0 || (end != usize::MAX && end == 0) {
                return Err(anyhow!("line numbers are 1-based: '{token}'"));
            }
            if end != usize::MAX && start > end {
                return Err(anyhow!("invalid line range (start > end): '{token}'"));
            }

            ranges.push((start, end));
        } else {
            let line: usize = token
                .parse()
                .map_err(|_| anyhow!("invalid line number: '{token}'"))?;
            if line == 0 {
                return Err(anyhow!("line numbers are 1-based: '{token}'"));
            }
            ranges.push((line, line));
        }
    }

    ranges.sort_by_key(|&(start, _)| start);

    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut()
            && last.1.saturating_add(1) >= start
        {
            last.1 = last.1.max(end);
            continue;
        }
        merged.push((start, end));
    }

    Ok(LineRanges(merged))
}

/// Apply edits sequentially to content.
///
/// Returns `(final_content, per_edit_counts)` where each count is the
/// number of replacements made by that edit.
#[must_use]
pub fn apply_edits(
    content: &str,
    edits: &[ParsedEdit],
    line_ranges: Option<&LineRanges>,
) -> (String, Vec<usize>) {
    let mut current = content.to_owned();
    let mut counts = Vec::with_capacity(edits.len());

    for edit in edits {
        let (new_content, count) = line_ranges.map_or_else(
            || apply_edit_whole(&current, edit),
            |lr| apply_edit_with_ranges(&current, edit, lr),
        );
        current = new_content;
        counts.push(count);
    }

    (current, counts)
}

/// Apply a single edit to the entire content string.
fn apply_edit_whole(content: &str, edit: &ParsedEdit) -> (String, usize) {
    match edit {
        ParsedEdit::Literal { old, new, global } => {
            if *global {
                if old == new {
                    return (content.to_owned(), 0);
                }
                let count = content.matches(old.as_str()).count();
                if count == 0 {
                    (content.to_owned(), 0)
                } else {
                    (content.replace(old.as_str(), new.as_str()), count)
                }
            } else if let Some(pos) = content.find(old.as_str()) {
                let mut result = String::with_capacity(content.len());
                result.push_str(&content[..pos]);
                result.push_str(new);
                result.push_str(&content[pos + old.len()..]);
                (result, 1)
            } else {
                (content.to_owned(), 0)
            }
        }
        ParsedEdit::Regex {
            pattern,
            replacement,
            global,
        } => {
            if *global {
                let count = pattern.find_iter(content).count();
                if count == 0 {
                    return (content.to_owned(), 0);
                }
                let result = pattern.replace_all(content, replacement.as_str());
                if *result == *content {
                    (content.to_owned(), 0)
                } else {
                    (result.into_owned(), count)
                }
            } else {
                let result = pattern.replace(content, replacement.as_str());
                if *result == *content {
                    (content.to_owned(), 0)
                } else {
                    (result.into_owned(), 1)
                }
            }
        }
    }
}

/// Apply a single edit line-by-line, only touching lines within the ranges.
fn apply_edit_with_ranges(
    content: &str,
    edit: &ParsedEdit,
    line_ranges: &LineRanges,
) -> (String, usize) {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let mut result_lines: Vec<String> = Vec::with_capacity(lines.len());
    let mut total_count = 0;

    for (i, line) in lines.iter().enumerate() {
        let line_num = i + 1;

        if line_ranges.contains(line_num) {
            let (new_line, count) = apply_edit_whole(line, edit);
            total_count += count;
            result_lines.push(new_line);
        } else {
            result_lines.push((*line).to_owned());
        }
    }

    (result_lines.concat(), total_count)
}

/// Returns `true` if the path passes through a VCS directory.
fn is_vcs_path(path: &Path) -> bool {
    path.components()
        .any(|c| matches!(c.as_os_str().to_str(), Some(".git" | ".hg" | ".svn")))
}

/// Returns `true` if the filename is a Catenary snapshot sidecar.
fn is_sidecar(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().contains(".catenary_snapshot_"))
}

/// Resolves the `glob` parameter to a list of target file paths.
///
/// - File path → single-element vec (rejects VCS paths).
/// - Directory → error with guidance.
/// - Glob pattern → walk with `ignore::WalkBuilder` across roots.
///
/// Applies `exclude`, `include_gitignored`, `include_hidden`.
/// Skips sidecar files and VCS directories.
///
/// # Errors
///
/// Returns an error if the path is inside a VCS directory, is a directory,
/// or if a glob/exclude pattern is invalid.
pub fn resolve_targets(
    glob_param: &str,
    roots: &[PathBuf],
    exclude: Option<&str>,
    include_gitignored: bool,
    include_hidden: bool,
) -> Result<Vec<PathBuf>> {
    let resolved = resolve_path(glob_param)?;

    if resolved.is_file() {
        if is_vcs_path(&resolved) {
            return Err(anyhow!("refusing to modify files inside .git/"));
        }
        return Ok(vec![resolved]);
    }

    if resolved.is_dir() {
        return Err(anyhow!(
            "{} is a directory — use a glob pattern (e.g., {}/*.rs)",
            resolved.display(),
            resolved.display()
        ));
    }

    // Treat as glob pattern.
    let matcher = Glob::new(glob_param)
        .map_err(|e| anyhow!("Invalid glob pattern: {e}"))?
        .compile_matcher();

    let exclude_matcher = exclude
        .map(|ex| {
            Glob::new(ex)
                .map_err(|e| anyhow!("Invalid exclude pattern: {e}"))
                .map(|g| g.compile_matcher())
        })
        .transpose()?;

    let search_roots = if roots.is_empty() {
        vec![
            std::env::current_dir()
                .map_err(|e| anyhow!("Failed to get current working directory: {e}"))?,
        ]
    } else {
        roots.to_vec()
    };

    // WalkBuilder flags use "skip" semantics: .hidden(true) = skip hidden
    let skip_gitignored = !include_gitignored;
    let skip_hidden = !include_hidden;

    let mut matched_files: Vec<PathBuf> = Vec::new();

    for root in &search_roots {
        let walker = WalkBuilder::new(root)
            .git_ignore(skip_gitignored)
            .hidden(skip_hidden)
            .build();

        for entry in walker.flatten() {
            let entry_path = entry.into_path();

            if !entry_path.is_file() {
                continue;
            }

            if is_vcs_path(&entry_path) || is_sidecar(&entry_path) {
                continue;
            }

            let rel_path = entry_path.strip_prefix(root).unwrap_or(&entry_path);

            if !matcher.is_match(rel_path) {
                continue;
            }

            if let Some(ref ex) = exclude_matcher
                && ex.is_match(rel_path)
            {
                continue;
            }

            matched_files.push(entry_path);
        }
    }

    matched_files.sort();
    matched_files.dedup();

    Ok(matched_files)
}

/// Reads file content as UTF-8. Returns `Err` for non-UTF-8 (binary) files.
///
/// # Errors
///
/// Returns an error if the file cannot be read or contains non-UTF-8 bytes.
pub fn read_file_utf8(path: &Path) -> Result<String> {
    let bytes =
        std::fs::read(path).map_err(|e| anyhow!("failed to read {}: {e}", path.display()))?;
    String::from_utf8(bytes).map_err(|_| anyhow!("not UTF-8: {}", path.display()))
}

/// Extracts line-level diffs between old and new content.
///
/// Returns `(old_line, new_line)` pairs from diff hunks using
/// the `similar` crate with patience diff algorithm.
#[must_use]
pub fn extract_diffs(old: &str, new: &str) -> Vec<(String, String)> {
    let diff = TextDiff::configure()
        .algorithm(similar::Algorithm::Patience)
        .diff_lines(old, new);

    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut deletes: Vec<String> = Vec::new();
    let mut inserts: Vec<String> = Vec::new();

    let flush = |deletes: &mut Vec<String>,
                 inserts: &mut Vec<String>,
                 pairs: &mut Vec<(String, String)>| {
        let max_len = deletes.len().max(inserts.len());
        for i in 0..max_len {
            let d = deletes.get(i).cloned().unwrap_or_default();
            let ins = inserts.get(i).cloned().unwrap_or_default();
            pairs.push((d, ins));
        }
        deletes.clear();
        inserts.clear();
    };

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                flush(&mut deletes, &mut inserts, &mut pairs);
            }
            ChangeTag::Delete => {
                if !inserts.is_empty() {
                    flush(&mut deletes, &mut inserts, &mut pairs);
                }
                deletes.push(change.value().trim_end_matches('\n').to_owned());
            }
            ChangeTag::Insert => {
                inserts.push(change.value().trim_end_matches('\n').to_owned());
            }
        }
    }

    flush(&mut deletes, &mut inserts, &mut pairs);

    pairs
}

/// Result of applying edits to a single file.
#[derive(Debug)]
pub struct ReplaceFileResult {
    /// Absolute path to the file.
    pub path: PathBuf,
    /// Total replacement count across all edits.
    pub count: usize,
    /// Per-edit replacement counts (parallel to input edits array).
    pub edit_counts: Vec<usize>,
    /// Snapshot ID if a snapshot was created (populated by ticket 01).
    pub snapshot_id: Option<i64>,
    /// Line-level diffs: `(old_line, new_line)` pairs.
    pub diffs: Vec<(String, String)>,
    /// Error message if this file failed or was skipped.
    pub error: Option<String>,
    /// New file content after edits (for ticket 01 to write).
    /// `None` if no changes or error.
    pub new_content: Option<String>,
}

/// Returns `true` if the error indicates a skipped (non-fatal) file.
fn is_skip_error(error: &str) -> bool {
    error == "not UTF-8"
}

/// Renders the per-file block for a single result.
///
/// Returns `None` if the file should be omitted (zero count, no error).
fn render_file_block(result: &ReplaceFileResult, max_samples: usize) -> Option<String> {
    if result.error.is_none() && result.count == 0 {
        return None;
    }

    let mut block = String::new();
    let path = result.path.display();

    if let Some(ref error) = result.error {
        if is_skip_error(error) {
            _ = writeln!(block, "{path}  (skipped: {error})");
        } else {
            _ = writeln!(block, "{path}  (error: {error})");
        }
        return Some(block);
    }

    // Header line.
    let s = if result.count == 1 {
        "replacement"
    } else {
        "replacements"
    };
    _ = write!(block, "{path}  ({} {s})", result.count);
    if let Some(id) = result.snapshot_id {
        _ = write!(block, "  [snapshot #{id}]");
    }
    block.push('\n');

    // Diff samples (only when tier includes diffs).
    if max_samples > 0 {
        let shown = result.diffs.len().min(max_samples);
        for (old, new) in &result.diffs[..shown] {
            _ = writeln!(block, "\t- {old}");
            _ = writeln!(block, "\t+ {new}");
        }
        let remaining = result.diffs.len() - shown;
        if remaining > 0 {
            _ = writeln!(block, "\t... ({remaining} more)");
        }
    }

    Some(block)
}

/// Builds the summary line for replace output.
fn render_summary(results: &[ReplaceFileResult]) -> String {
    let total_count: usize = results.iter().map(|r| r.count).sum();
    let success_files = results
        .iter()
        .filter(|r| r.count > 0 && r.error.is_none())
        .count();
    let skipped = results
        .iter()
        .filter(|r| r.error.as_deref().is_some_and(is_skip_error))
        .count();
    let errors = results
        .iter()
        .filter(|r| r.error.as_deref().is_some_and(|e| !is_skip_error(e)))
        .count();

    let mut line = if total_count == 0 {
        "0 replacements".to_owned()
    } else {
        let r = if total_count == 1 {
            "replacement"
        } else {
            "replacements"
        };
        if results.len() > 1 {
            let f = if success_files == 1 { "file" } else { "files" };
            format!("{total_count} total {r} across {success_files} {f}")
        } else {
            format!("{total_count} total {r}")
        }
    };

    if skipped > 0 || errors > 0 {
        let mut parts = Vec::new();
        if skipped > 0 {
            parts.push(format!("{skipped} skipped"));
        }
        if errors > 0 {
            parts.push(format!("{errors} error"));
        }
        _ = write!(line, " ({})", parts.join(", "));
    }

    line
}

/// Renders replace results into budget-constrained output text.
///
/// Uses promote-from-bottom: renders minimal (counts only), then
/// tries reduced (1 diff sample per file), then full (3 samples).
/// The `diagnostics` string (from `DiagnosticsServer::process_files`)
/// is always included and never trimmed.
#[must_use]
pub fn render_replace_output(
    results: &[ReplaceFileResult],
    budget: u32,
    diagnostics: &str,
) -> String {
    let summary = render_summary(results);

    let assemble = |max_samples: usize| -> String {
        let mut out = String::new();
        for result in results {
            if let Some(block) = render_file_block(result, max_samples) {
                out.push_str(&block);
            }
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&summary);
        if !diagnostics.is_empty() {
            out.push('\n');
            out.push_str(diagnostics);
        }
        out
    };

    let full = assemble(3);
    if full.len() <= budget as usize {
        return full;
    }

    let reduced = assemble(1);
    if reduced.len() <= budget as usize {
        return reduced;
    }

    assemble(0)
}

/// Processes a replace operation end-to-end.
///
/// Resolves targets, reads files, applies edits, extracts diffs,
/// and renders output. Does not create snapshots or collect LSP
/// diagnostics (ticket 01).
///
/// `roots` are the workspace roots for glob resolution.
///
/// # Errors
///
/// Returns an error if edit parsing fails, line range parsing fails,
/// target resolution fails, or a single-target file cannot be read.
pub fn process_replace(
    input: &ReplaceInput,
    roots: &[PathBuf],
) -> Result<(Vec<ReplaceFileResult>, String)> {
    let parsed_edits = parse_edits(&input.edits)?;

    let line_ranges = input.lines.as_deref().map(parse_line_ranges).transpose()?;

    let targets = resolve_targets(
        &input.glob,
        roots,
        input.exclude.as_deref(),
        input.include_gitignored,
        input.include_hidden,
    )?;

    let is_multi = targets.len() > 1;
    let mut results = Vec::with_capacity(targets.len());

    for path in targets {
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                if !is_multi {
                    return Err(anyhow!("failed to read {}: {e}", path.display()));
                }
                results.push(ReplaceFileResult {
                    path,
                    count: 0,
                    edit_counts: vec![],
                    snapshot_id: None,
                    diffs: vec![],

                    error: Some(e.to_string()),
                    new_content: None,
                });
                continue;
            }
        };

        let Ok(content) = String::from_utf8(bytes) else {
            if !is_multi {
                return Err(anyhow!("not UTF-8: {}", path.display()));
            }
            results.push(ReplaceFileResult {
                path,
                count: 0,
                edit_counts: vec![],
                snapshot_id: None,
                diffs: vec![],

                error: Some("not UTF-8".to_owned()),
                new_content: None,
            });
            continue;
        };

        let (new_content, edit_counts) = apply_edits(&content, &parsed_edits, line_ranges.as_ref());

        if new_content == content {
            results.push(ReplaceFileResult {
                path,
                count: 0,
                edit_counts,
                snapshot_id: None,
                diffs: vec![],

                error: None,
                new_content: None,
            });
            continue;
        }

        let count: usize = edit_counts.iter().sum();
        let diffs = extract_diffs(&content, &new_content);

        results.push(ReplaceFileResult {
            path,
            count,
            edit_counts,
            snapshot_id: None,
            diffs,
            error: None,
            new_content: Some(new_content),
        });
    }

    let output = render_replace_output(&results, 4000, "");
    Ok((results, output))
}

// ─── ReplaceServer ───────────────────────────────────────────────────────

use super::diagnostics_server::DiagnosticsServer;
use super::tool_server::ToolServer;
use crate::lsp::ClientManager;
use rusqlite::Connection;
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::Mutex;

use super::DocumentManager;

/// Batch replacement tool with snapshots and diagnostics.
///
/// Implements `ToolServer` — a black box in the transformation layer.
/// Receives edit parameters, applies them across files, creates `SQLite`
/// snapshots before writes, and collects LSP diagnostics after writes
/// via the shared `DiagnosticsServer`.
pub struct ReplaceServer {
    client_manager: Arc<ClientManager>,
    #[allow(dead_code, reason = "reserved for future document sync integration")]
    doc_manager: Arc<Mutex<DocumentManager>>,
    diagnostics: Arc<DiagnosticsServer>,
    #[allow(
        dead_code,
        reason = "reserved for future blocking dispatch from sync contexts"
    )]
    runtime: Handle,
    session_id: Option<String>,
}

impl ReplaceServer {
    /// Creates a new `ReplaceServer`.
    pub const fn new(
        client_manager: Arc<ClientManager>,
        doc_manager: Arc<Mutex<DocumentManager>>,
        diagnostics: Arc<DiagnosticsServer>,
        runtime: Handle,
        session_id: Option<String>,
    ) -> Self {
        Self {
            client_manager,
            doc_manager,
            diagnostics,
            runtime,
            session_id,
        }
    }
}

impl ToolServer for ReplaceServer {
    async fn execute(
        &self,
        params: &serde_json::Value,
        parent_id: Option<i64>,
    ) -> anyhow::Result<serde_json::Value> {
        let input: ReplaceInput = serde_json::from_value(params.clone())
            .map_err(|e| anyhow!("invalid replace parameters: {e}"))?;

        let roots = self.client_manager.roots().await;
        let (mut results, _) = process_replace(&input, &roots)?;

        // Open a database connection for snapshot creation.
        let conn = crate::db::open()?;

        let entry_id = parent_id.unwrap_or(0);

        for result in &mut results {
            let Some(ref new_content) = result.new_content else {
                continue;
            };

            // Read original content for the snapshot.
            let original = std::fs::read(&result.path)
                .map_err(|e| anyhow!("failed to read {} for snapshot: {e}", result.path.display()));

            let original = match original {
                Ok(bytes) => bytes,
                Err(e) => {
                    result.error = Some(e.to_string());
                    result.new_content = None;
                    continue;
                }
            };

            // Create snapshot — if this fails, abort for this file.
            let snapshot_id = match create_snapshot(
                &conn,
                &result.path,
                &original,
                result.edit_counts.len(),
                result.count,
                self.session_id.as_deref(),
            ) {
                Ok(id) => id,
                Err(e) => {
                    result.error = Some(format!("snapshot failed: {e}. File not modified."));
                    result.new_content = None;
                    continue;
                }
            };

            result.snapshot_id = Some(snapshot_id);

            // Write new content preserving file permissions.
            let write_result = write_preserving_permissions(&result.path, new_content.as_bytes());
            if let Err(e) = write_result {
                result.error = Some(format!("write failed: {e}"));
                result.new_content = None;
            }
        }

        // Collect diagnostics for all modified files (best-effort).
        let modified_paths: Vec<String> = results
            .iter()
            .filter(|r| r.snapshot_id.is_some() && r.error.is_none())
            .map(|r| r.path.to_string_lossy().into_owned())
            .collect();
        let path_refs: Vec<&str> = modified_paths.iter().map(String::as_str).collect();
        let diagnostics = self.diagnostics.process_files(&path_refs, entry_id).await;

        let output = render_replace_output(&results, 4000, &diagnostics);
        Ok(serde_json::Value::String(output))
    }
}

/// Creates a snapshot of the file's original content before modification.
///
/// Returns the snapshot row ID on success.
///
/// # Errors
///
/// Returns an error if the database insert fails.
fn create_snapshot(
    conn: &Connection,
    file_path: &Path,
    content: &[u8],
    edit_count: usize,
    replacement_count: usize,
    session_id: Option<&str>,
) -> Result<i64> {
    let pattern = format!("{edit_count} edits");

    conn.execute(
        "INSERT INTO snapshots \
             (file_path, content, source, pattern, replacement, count, created_at, session_id) \
         VALUES (?1, ?2, 'replace', ?3, NULL, ?4, datetime('now'), ?5)",
        rusqlite::params![
            file_path.to_string_lossy().as_ref(),
            content,
            pattern,
            replacement_count,
            session_id,
        ],
    )
    .map_err(|e| anyhow!("failed to insert snapshot: {e}"))?;

    Ok(conn.last_insert_rowid())
}

/// Writes content to a file, preserving the original file permissions.
///
/// # Errors
///
/// Returns an error if reading metadata, writing content, or setting
/// permissions fails.
fn write_preserving_permissions(path: &Path, content: &[u8]) -> Result<()> {
    let metadata = std::fs::metadata(path)
        .map_err(|e| anyhow!("failed to read metadata for {}: {e}", path.display()))?;
    let permissions = metadata.permissions();
    std::fs::write(path, content)
        .map_err(|e| anyhow!("failed to write {}: {e}", path.display()))?;
    std::fs::set_permissions(path, permissions)
        .map_err(|e| anyhow!("failed to set permissions on {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    fn make_edit(old: &str, new: &str, flags: Option<&str>) -> EditEntry {
        EditEntry {
            old: old.to_owned(),
            new: new.to_owned(),
            flags: flags.map(String::from),
        }
    }

    // --- Flag parsing ---

    #[test]
    fn test_flags_none() {
        let f = parse_flags(None, 0).expect("should parse None flags");
        assert!(!f.global);
        assert!(!f.regex);
        assert!(!f.case_insensitive);
        assert!(!f.multiline);
        assert!(!f.dotall);
    }

    #[test]
    fn test_flags_g() {
        let f = parse_flags(Some("g"), 0).expect("should parse 'g'");
        assert!(f.global);
        assert!(!f.regex);
    }

    #[test]
    fn test_flags_r() {
        let f = parse_flags(Some("r"), 0).expect("should parse 'r'");
        assert!(f.regex);
        assert!(!f.global);
    }

    #[test]
    fn test_flags_rg() {
        let f = parse_flags(Some("rg"), 0).expect("should parse 'rg'");
        assert!(f.regex);
        assert!(f.global);
    }

    #[test]
    fn test_flags_i_implies_r() {
        let f = parse_flags(Some("i"), 0).expect("should parse 'i'");
        assert!(f.regex);
        assert!(f.case_insensitive);
    }

    #[test]
    fn test_flags_invalid() {
        let err = parse_flags(Some("gx"), 0).expect_err("should reject 'x'");
        let msg = err.to_string();
        assert!(msg.contains("unknown flag 'x'"), "got: {msg}");
    }

    // --- Replacement normalization ---

    #[test]
    fn test_normalize_bare_numeric() {
        assert_eq!(normalize_replacement("$2_$1", 0).expect("ok"), "${2}_${1}");
    }

    #[test]
    fn test_normalize_escaped_dollar() {
        assert_eq!(normalize_replacement(r"\$2_$1", 0).expect("ok"), "$$2_${1}");
    }

    #[test]
    fn test_normalize_escaped_backslash() {
        assert_eq!(
            normalize_replacement(r"\\$2_$1", 0).expect("ok"),
            r"\${2}_${1}"
        );
    }

    #[test]
    fn test_normalize_already_braced() {
        assert_eq!(
            normalize_replacement("${2}_${1}", 0).expect("ok"),
            "${2}_${1}"
        );
    }

    #[test]
    fn test_normalize_no_specials() {
        assert_eq!(
            normalize_replacement("hello world", 0).expect("ok"),
            "hello world"
        );
    }

    #[test]
    fn test_normalize_backslash_passthrough() {
        assert_eq!(normalize_replacement(r"\n", 0).expect("ok"), r"\n");
    }

    #[test]
    fn test_normalize_bare_dollar_error() {
        let err = normalize_replacement("$$2", 0).expect_err("should reject bare $");
        assert!(err.to_string().contains("invalid '$'"), "got: {err}");
    }

    #[test]
    fn test_normalize_trailing_dollar_error() {
        let err = normalize_replacement("foo$", 0).expect_err("should reject trailing $");
        assert!(err.to_string().contains("trailing '$'"), "got: {err}");
    }

    #[test]
    fn test_normalize_named_group_error() {
        let err = normalize_replacement("${name}", 0).expect_err("should reject named group");
        assert!(
            err.to_string().contains("named capture groups"),
            "got: {err}"
        );
    }

    // --- Edit application ---

    #[test]
    fn test_literal_first_match() {
        let edits = parse_edits(&[make_edit("foo", "baz", None)]).expect("parse");
        let (result, counts) = apply_edits("foo bar foo", &edits, None);
        assert_eq!(result, "baz bar foo");
        assert_eq!(counts, vec![1]);
    }

    #[test]
    fn test_literal_global() {
        let edits = parse_edits(&[make_edit("foo", "baz", Some("g"))]).expect("parse");
        let (result, counts) = apply_edits("foo bar foo", &edits, None);
        assert_eq!(result, "baz bar baz");
        assert_eq!(counts, vec![2]);
    }

    #[test]
    fn test_regex_first_match() {
        let edits = parse_edits(&[make_edit(r"\d+", "N", Some("r"))]).expect("parse");
        let (result, counts) = apply_edits("foo123 bar456", &edits, None);
        assert_eq!(result, "fooN bar456");
        assert_eq!(counts, vec![1]);
    }

    #[test]
    fn test_regex_global() {
        let edits = parse_edits(&[make_edit(r"\d+", "N", Some("rg"))]).expect("parse");
        let (result, counts) = apply_edits("foo123 bar456", &edits, None);
        assert_eq!(result, "fooN barN");
        assert_eq!(counts, vec![2]);
    }

    #[test]
    fn test_capture_groups() {
        let edits = parse_edits(&[make_edit(r"(\w+)_(\w+)", "$2_$1", Some("r"))]).expect("parse");
        let (result, counts) = apply_edits("hello_world", &edits, None);
        assert_eq!(result, "world_hello");
        assert_eq!(counts, vec![1]);
    }

    #[test]
    fn test_multiple_capture_groups() {
        let edits =
            parse_edits(&[make_edit(r"v(\d+)\.(\d+)", "V${1}_${2}", Some("r"))]).expect("parse");
        let (result, counts) = apply_edits("v1.2.3", &edits, None);
        assert_eq!(result, "V1_2.3");
        assert_eq!(counts, vec![1]);
    }

    #[test]
    fn test_case_insensitive() {
        let edits = parse_edits(&[make_edit("foo", "bar", Some("gi"))]).expect("parse");
        let (result, counts) = apply_edits("Foo FOO foo", &edits, None);
        assert_eq!(result, "bar bar bar");
        assert_eq!(counts, vec![3]);
    }

    #[test]
    fn test_sequential_edits() {
        let edits =
            parse_edits(&[make_edit("A", "B", None), make_edit("B", "C", None)]).expect("parse");
        let (result, counts) = apply_edits("A", &edits, None);
        assert_eq!(result, "C");
        assert_eq!(counts, vec![1, 1]);
    }

    #[test]
    fn test_no_matches() {
        let edits = parse_edits(&[make_edit("xyz", "abc", None)]).expect("parse");
        let (result, counts) = apply_edits("hello world", &edits, None);
        assert_eq!(result, "hello world");
        assert_eq!(counts, vec![0]);
    }

    #[test]
    fn test_empty_replacement() {
        let edits = parse_edits(&[make_edit("foo", "", Some("g"))]).expect("parse");
        let (result, counts) = apply_edits("foo bar foo", &edits, None);
        assert_eq!(result, " bar ");
        assert_eq!(counts, vec![2]);
    }

    #[test]
    fn test_idempotent() {
        let edits = parse_edits(&[make_edit("foo", "foo", Some("g"))]).expect("parse");
        let (result, counts) = apply_edits("foo bar foo", &edits, None);
        assert_eq!(result, "foo bar foo");
        assert_eq!(counts, vec![0]);
    }

    // --- Line ranges ---

    #[test]
    fn test_lines_single_range() {
        let lr = parse_line_ranges("1-3").expect("parse");
        let edits = parse_edits(&[make_edit("x", "y", Some("g"))]).expect("parse");
        let content = "x one\nx two\nx three\nx four\nx five\n";
        let (result, counts) = apply_edits(content, &edits, Some(&lr));
        assert_eq!(result, "y one\ny two\ny three\nx four\nx five\n");
        assert_eq!(counts, vec![3]);
    }

    #[test]
    fn test_lines_multiple_ranges() {
        let lr = parse_line_ranges("1-2 4-5").expect("parse");
        let edits = parse_edits(&[make_edit("x", "y", Some("g"))]).expect("parse");
        let content = "x one\nx two\nx three\nx four\nx five\n";
        let (result, counts) = apply_edits(content, &edits, Some(&lr));
        assert_eq!(result, "y one\ny two\nx three\ny four\ny five\n");
        assert_eq!(counts, vec![4]);
    }

    #[test]
    fn test_lines_single_line() {
        let lr = parse_line_ranges("3").expect("parse");
        let edits = parse_edits(&[make_edit("x", "y", Some("g"))]).expect("parse");
        let content = "x one\nx two\nx three\nx four\nx five\n";
        let (result, counts) = apply_edits(content, &edits, Some(&lr));
        assert_eq!(result, "x one\nx two\ny three\nx four\nx five\n");
        assert_eq!(counts, vec![1]);
    }

    #[test]
    fn test_lines_open_ended() {
        let lr = parse_line_ranges("3-").expect("parse");
        let edits = parse_edits(&[make_edit("x", "y", Some("g"))]).expect("parse");
        let content = "x one\nx two\nx three\nx four\nx five\n";
        let (result, counts) = apply_edits(content, &edits, Some(&lr));
        assert_eq!(result, "x one\nx two\ny three\ny four\ny five\n");
        assert_eq!(counts, vec![3]);
    }

    #[test]
    fn test_lines_overlap_merge() {
        let lr = parse_line_ranges("1-5 3-8").expect("parse");
        for line in 1..=8 {
            assert!(lr.contains(line), "line {line} should be in range");
        }
        assert!(!lr.contains(9), "line 9 should not be in range");
    }

    // --- File scoping ---

    #[test]
    fn test_resolve_single_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("hello.rs");
        std::fs::write(&file, "fn main() {}").expect("write");

        let result = resolve_targets(file.to_str().expect("utf8"), &[], None, false, false)
            .expect("resolve");
        assert_eq!(result, vec![file]);
    }

    #[test]
    fn test_resolve_directory_error() {
        let dir = tempfile::tempdir().expect("tempdir");

        let err = resolve_targets(dir.path().to_str().expect("utf8"), &[], None, false, false)
            .expect_err("should error on directory");
        let msg = err.to_string();
        assert!(msg.contains("is a directory"), "got: {msg}");
    }

    #[test]
    fn test_resolve_glob_pattern() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "a").expect("write");
        std::fs::write(dir.path().join("b.rs"), "b").expect("write");
        std::fs::write(dir.path().join("c.txt"), "c").expect("write");

        let roots = vec![dir.path().to_path_buf()];
        let result = resolve_targets("*.rs", &roots, None, false, false).expect("resolve");
        assert_eq!(result.len(), 2);
        assert!(
            result
                .iter()
                .all(|p| p.extension().is_some_and(|e| e == "rs"))
        );
    }

    #[test]
    fn test_resolve_vcs_rejection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git_dir = dir.path().join(".git");
        std::fs::create_dir(&git_dir).expect("mkdir");
        let config = git_dir.join("config");
        std::fs::write(&config, "[core]").expect("write");

        let err = resolve_targets(config.to_str().expect("utf8"), &[], None, false, false)
            .expect_err("should reject VCS path");
        let msg = err.to_string();
        assert!(msg.contains(".git"), "got: {msg}");
    }

    #[test]
    fn test_resolve_sidecar_exclusion() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("foo.rs"), "code").expect("write");
        std::fs::write(dir.path().join("foo.catenary_snapshot_1.rs"), "snapshot").expect("write");

        let roots = vec![dir.path().to_path_buf()];
        let result = resolve_targets("*.rs", &roots, None, false, false).expect("resolve");
        assert_eq!(result.len(), 1);
        assert!(
            result[0].file_name().expect("name").to_string_lossy() == "foo.rs",
            "got: {result:?}"
        );
    }

    #[test]
    fn test_resolve_exclude() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("src.rs"), "code").expect("write");
        std::fs::write(dir.path().join("test_src.rs"), "test").expect("write");

        let roots = vec![dir.path().to_path_buf()];
        let result =
            resolve_targets("*.rs", &roots, Some("test_*"), false, false).expect("resolve");
        assert_eq!(result.len(), 1);
        assert!(
            result[0].file_name().expect("name").to_string_lossy() == "src.rs",
            "got: {result:?}"
        );
    }

    // --- Diff extraction ---

    #[test]
    fn test_diff_simple() {
        let old = "line1\nline2\n";
        let new = "line1\nchanged\n";
        let pairs = extract_diffs(old, new);
        assert_eq!(pairs, vec![("line2".to_owned(), "changed".to_owned())]);
    }

    #[test]
    fn test_diff_multi_line() {
        let old = "one\ntwo\nthree\nfour\nfive\n";
        let new = "one\nTWO\nthree\nFOUR\nfive\n";
        let pairs = extract_diffs(old, new);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("two".to_owned(), "TWO".to_owned()));
        assert_eq!(pairs[1], ("four".to_owned(), "FOUR".to_owned()));
    }

    #[test]
    fn test_lines_out_of_bounds() {
        let lr = parse_line_ranges("100-200").expect("parse");
        let edits = parse_edits(&[make_edit("x", "y", Some("g"))]).expect("parse");
        let content = "x one\nx two\nx three\nx four\nx five\n";
        let (result, counts) = apply_edits(content, &edits, Some(&lr));
        assert_eq!(result, content);
        assert_eq!(counts, vec![0]);
    }

    // --- Output rendering ---

    fn make_result(
        path: &str,
        count: usize,
        diffs: Vec<(String, String)>,
        error: Option<&str>,
    ) -> ReplaceFileResult {
        ReplaceFileResult {
            path: PathBuf::from(path),
            count,
            edit_counts: vec![count],
            snapshot_id: None,
            diffs,
            error: error.map(String::from),
            new_content: None,
        }
    }

    #[test]
    fn test_render_single_file() {
        let result = make_result(
            "src/handler.rs",
            8,
            vec![
                ("use crate::old".to_owned(), "use crate::new".to_owned()),
                ("OldType".to_owned(), "NewType".to_owned()),
                ("old_fn()".to_owned(), "new_fn()".to_owned()),
            ],
            None,
        );

        let output = render_replace_output(&[result], 4000, "");
        assert!(output.contains("(8 replacements)"), "got: {output}");
        assert!(output.contains("\t- use crate::old"), "got: {output}");
        assert!(output.contains("\t+ use crate::new"), "got: {output}");
        assert!(output.contains("8 total replacements"), "got: {output}");
        assert!(
            !output.contains("[snapshot"),
            "no snapshot expected, got: {output}"
        );
    }

    #[test]
    fn test_render_multi_file() {
        let results = vec![
            make_result("src/a.rs", 5, vec![("old".into(), "new".into())], None),
            make_result("src/b.rs", 3, vec![("old".into(), "new".into())], None),
            make_result("src/c.rs", 2, vec![("old".into(), "new".into())], None),
        ];

        let output = render_replace_output(&results, 4000, "");
        assert!(output.contains("src/a.rs"), "got: {output}");
        assert!(output.contains("src/b.rs"), "got: {output}");
        assert!(output.contains("src/c.rs"), "got: {output}");
        assert!(
            output.contains("10 total replacements across 3 files"),
            "got: {output}"
        );
    }

    #[test]
    fn test_render_no_matches() {
        let result = make_result("src/handler.rs", 0, vec![], None);
        let output = render_replace_output(&[result], 4000, "");
        assert_eq!(output, "0 replacements");
    }

    #[test]
    fn test_render_budget_tiers() {
        let diffs: Vec<(String, String)> = (0..10)
            .map(|i| {
                (
                    format!("old_function_call(arg1, arg2, arg3) // line {i}"),
                    format!("new_function_call(arg1, arg2, arg3) // line {i}"),
                )
            })
            .collect();
        let results = [make_result("src/handler.rs", 10, diffs, None)];

        // Full tier: large budget fits all 3 samples.
        let full = render_replace_output(&results, 50_000, "");
        let full_samples = full.matches("\t- ").count();
        assert_eq!(
            full_samples, 3,
            "full tier should show 3 samples, got:\n{full}"
        );

        // Reduced tier: budget too small for full, fits 1 sample.
        let reduced = render_replace_output(&results, 200, "");
        let reduced_samples = reduced.matches("\t- ").count();
        assert_eq!(
            reduced_samples, 1,
            "reduced tier should show 1 sample, got:\n{reduced}"
        );

        // Minimal tier: budget too small for reduced, counts only.
        let minimal = render_replace_output(&results, 50, "");
        let minimal_samples = minimal.matches("\t- ").count();
        assert_eq!(
            minimal_samples, 0,
            "minimal tier should show 0 samples, got:\n{minimal}"
        );
    }

    #[test]
    fn test_render_errors() {
        let results = vec![
            make_result(
                "src/handler.rs",
                8,
                vec![("old".into(), "new".into())],
                None,
            ),
            make_result("src/binary.dat", 0, vec![], Some("not UTF-8")),
            make_result("src/readonly.rs", 0, vec![], Some("permission denied")),
        ];

        let output = render_replace_output(&results, 4000, "");
        assert!(output.contains("(skipped: not UTF-8)"), "got: {output}");
        assert!(
            output.contains("(error: permission denied)"),
            "got: {output}"
        );
        assert!(output.contains("(1 skipped, 1 error)"), "got: {output}");
    }

    // --- Snapshot tests ---

    fn open_test_db() -> rusqlite::Connection {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.db");
        let conn = crate::db::open_and_migrate_at(&path).expect("open_and_migrate_at");
        // Leak the tempdir so the database file persists for the test.
        std::mem::forget(dir);
        conn
    }

    #[test]
    fn test_snapshot_created() {
        let conn = open_test_db();
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("test.rs");
        let content = b"fn main() {}";
        std::fs::write(&file, content).expect("write");

        let id = create_snapshot(&conn, &file, content, 2, 5, None).expect("snapshot");
        assert!(id > 0, "snapshot id should be positive");

        let (file_path, source, count): (String, String, i64) = conn
            .query_row(
                "SELECT file_path, source, count FROM snapshots WHERE id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get(0).expect("col0"),
                        row.get(1).expect("col1"),
                        row.get(2).expect("col2"),
                    ))
                },
            )
            .expect("query");
        assert_eq!(file_path, file.to_string_lossy().as_ref());
        assert_eq!(source, "replace");
        assert_eq!(count, 5);
    }

    #[test]
    fn test_snapshot_content() {
        let conn = open_test_db();
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("test.rs");
        let content = b"fn main() { println!(\"hello\"); }";
        std::fs::write(&file, content).expect("write");

        let id =
            create_snapshot(&conn, &file, content, 1, 1, Some("test-session")).expect("snapshot");

        let (blob, session_id): (Vec<u8>, Option<String>) = conn
            .query_row(
                "SELECT content, session_id FROM snapshots WHERE id = ?1",
                [id],
                |row| Ok((row.get(0).expect("col0"), row.get(1).expect("col1"))),
            )
            .expect("query");
        assert_eq!(blob, content);
        assert_eq!(session_id.as_deref(), Some("test-session"));
    }

    #[test]
    fn test_multi_file_snapshots() {
        let conn = open_test_db();
        let dir = tempfile::tempdir().expect("tempdir");

        for name in &["a.rs", "b.rs", "c.rs"] {
            let file = dir.path().join(name);
            let content = format!("// {name}");
            std::fs::write(&file, &content).expect("write");
            create_snapshot(&conn, &file, content.as_bytes(), 1, 1, None).expect("snapshot");
        }

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 3);
    }

    #[test]
    fn test_no_snapshot_on_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "fn main() {}").expect("write");

        let input = ReplaceInput {
            glob: file.to_string_lossy().to_string(),
            edits: vec![make_edit("nonexistent", "replacement", None)],
            lines: None,
            exclude: None,
            include_gitignored: false,
            include_hidden: false,
        };

        let (results, _) = process_replace(&input, &[]).expect("process");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].count, 0);
        assert!(results[0].snapshot_id.is_none());
    }

    #[test]
    fn test_snapshot_id_in_output() {
        let mut result = make_result(
            "src/handler.rs",
            8,
            vec![("old".into(), "new".into())],
            None,
        );
        result.snapshot_id = Some(42);

        let output = render_replace_output(&[result], 4000, "");
        assert!(
            output.contains("[snapshot #42]"),
            "should contain snapshot id, got: {output}"
        );
    }

    // --- Diagnostics rendering ---

    #[test]
    fn test_diagnostics_in_output() {
        let result = make_result(
            "src/handler.rs",
            3,
            vec![("old".into(), "new".into())],
            None,
        );
        let diags =
            "diagnostics:\n\tsrc/handler.rs\n\t:10:5 [error] rustc(E0308): mismatched types\n";

        let output = render_replace_output(&[result], 4000, diags);
        assert!(
            output.contains("diagnostics:"),
            "should contain diagnostics section, got: {output}"
        );
        assert!(
            output.contains("E0308"),
            "should contain error code, got: {output}"
        );
    }

    #[test]
    fn test_diagnostics_empty_omitted() {
        let result = make_result(
            "src/handler.rs",
            3,
            vec![("old".into(), "new".into())],
            None,
        );

        let output = render_replace_output(&[result], 4000, "");
        assert!(
            !output.contains("diagnostics:"),
            "empty diagnostics should not produce diagnostics section, got: {output}"
        );
    }

    #[test]
    fn test_snapshot_pattern_format() {
        let conn = open_test_db();
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "content").expect("write");

        let id = create_snapshot(&conn, &file, b"content", 4, 10, None).expect("snapshot");

        let pattern: String = conn
            .query_row("SELECT pattern FROM snapshots WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .expect("query");
        assert_eq!(pattern, "4 edits");
    }
}

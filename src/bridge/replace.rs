// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Replace tool core: input parsing, flag validation, edit application.

use anyhow::{Result, anyhow};
use regex::Regex;
use serde::Deserialize;

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

    #[test]
    fn test_lines_out_of_bounds() {
        let lr = parse_line_ranges("100-200").expect("parse");
        let edits = parse_edits(&[make_edit("x", "y", Some("g"))]).expect("parse");
        let content = "x one\nx two\nx three\nx four\nx five\n";
        let (result, counts) = apply_edits(content, &edits, Some(&lr));
        assert_eq!(result, content);
        assert_eq!(counts, vec![0]);
    }
}

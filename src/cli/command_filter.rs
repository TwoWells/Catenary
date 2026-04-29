// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Shell command parser for allowlist-based command filtering.
//!
//! Checks Bash commands against a [`ResolvedCommands`] allowlist. Reimplements
//! all parsing logic from `scripts/constrained_bash.py` in Rust: pipeline
//! position tracking, subshell recursion, heredoc exception, quote-aware
//! splitting, env var prefix skipping, full path stripping, and subcommand
//! deny matching.

#[allow(
    clippy::expect_used,
    reason = "all patterns are string literals verified by tests — no user input"
)]
mod patterns {
    use regex::Regex;
    use std::sync::LazyLock;

    /// Matches `$(...)`, `<(...)`, and `` `...` `` substitutions for recursive checking.
    pub static SUBSHELL_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\$\(([^)]*)\)|<\(([^)]*)\)|`([^`]*)`").expect("constant pattern")
    });

    /// Matches heredoc start markers: `<<EOF`, `<<'EOF'`, `<<"EOF"`, `<<-EOF`.
    pub static HEREDOC_MARKER_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"<<-?\s*\\?['""]?(\w+)['""]?"#).expect("constant pattern"));

    /// Splits on sequential operators: `&&`, `||`, `;`.
    pub static SEQ_SPLIT_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\s*(?:&&|\|\||;)\s*").expect("constant pattern"));

    /// Matches env var assignment prefix: `VAR=value`.
    pub static ENV_VAR_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z_0-9]*=").expect("constant pattern"));

    /// Echo separator between sequential operators.
    pub static ECHO_SEP_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(&&|\|\||;)\s*echo\s+(?:"[^"]*"|'[^']*')\s*(&&|\|\||;)"#)
            .expect("constant pattern")
    });
}
use patterns::{ECHO_SEP_RE, ENV_VAR_RE, HEREDOC_MARKER_RE, SEQ_SPLIT_RE, SUBSHELL_RE};

use regex::Regex;

use crate::config::ResolvedCommands;

/// Replace quoted content (including delimiters) with spaces.
///
/// Preserves string length and character positions so that regex
/// matches on the masked string can be mapped back to the original.
/// Prevents operators inside quoted strings from being treated as
/// shell operators.
fn mask_quotes(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = vec![b' '; bytes.len()];
    let n = bytes.len();
    let mut i = 0;

    while i < n {
        if bytes[i] == b'\'' {
            let j = memchr::memchr(b'\'', &bytes[i + 1..]).map_or(n - 1, |offset| i + 1 + offset);
            i = j + 1;
        } else if bytes[i] == b'"' {
            let mut j = i + 1;
            while j < n && bytes[j] != b'"' {
                if bytes[j] == b'\\' && j + 1 < n {
                    j += 1;
                }
                j += 1;
            }
            i = j + 1;
        } else {
            out[i] = bytes[i];
            i += 1;
        }
    }

    String::from_utf8(out).unwrap_or_else(|_| " ".repeat(n))
}

/// Split `cmd` on `sep_re`, ignoring matches inside quoted strings.
fn quote_aware_split<'a>(cmd: &'a str, sep_re: &Regex) -> Vec<&'a str> {
    let masked = mask_quotes(cmd);
    let mut parts = Vec::new();
    let mut last = 0;
    for m in sep_re.find_iter(&masked) {
        parts.push(&cmd[last..m.start()]);
        last = m.end();
    }
    parts.push(&cmd[last..]);
    parts
}

/// Split `cmd` on bare `|` (not `||`), ignoring operators inside quotes.
///
/// Rust's `regex` crate does not support lookahead/lookbehind, so this
/// uses character-level scanning on the quote-masked string instead.
fn pipe_split(cmd: &str) -> Vec<&str> {
    let masked = mask_quotes(cmd);
    let bytes = masked.as_bytes();
    let n = bytes.len();
    let mut parts = Vec::new();
    let mut last = 0;
    let mut i = 0;

    while i < n {
        if bytes[i] == b'|' {
            // Skip || (logical OR) — not a pipe
            if i + 1 < n && bytes[i + 1] == b'|' {
                i += 2;
                continue;
            }
            // Check this isn't the second | of a || we already skipped past
            if i > 0 && bytes[i - 1] == b'|' {
                i += 1;
                continue;
            }
            // Bare pipe: split here
            let end = cmd[last..i].trim_end().len() + last;
            parts.push(&cmd[last..end]);
            i += 1;
            while i < n && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            last = i;
            continue;
        }
        i += 1;
    }
    parts.push(&cmd[last..]);
    parts
}

/// Strip echo separators between sequential operators.
///
/// Agents insert `&& echo "---" &&` as visual separators. This replaces
/// those patterns with just the operators so they don't interfere with
/// command checking.
fn strip_echo_separators(s: &str) -> String {
    let mut result = s.to_string();
    loop {
        let next = ECHO_SEP_RE.replace(&result, "$1 $2").to_string();
        if next == result {
            break;
        }
        result = next;
    }
    result
}

/// Remove heredoc bodies, keeping the marker line and closing delimiter.
///
/// Heredoc bodies are literal text, not shell commands. Without stripping
/// them, the recursive subshell checker would parse their content as
/// commands — triggering false denials on natural language.
fn strip_heredoc_bodies(cmd_string: &str) -> String {
    let mut result = Vec::new();
    let mut skip_until: Option<String> = None;

    for line in cmd_string.split('\n') {
        if let Some(ref marker) = skip_until {
            if line.trim() == marker {
                skip_until = None;
                result.push(line);
            }
            continue;
        }
        result.push(line);
        if let Some(m) = HEREDOC_MARKER_RE.captures(line)
            && let Some(marker) = m.get(1)
        {
            skip_until = Some(marker.as_str().to_string());
        }
    }
    result.join("\n")
}

/// Skip leading environment variable assignments to find the command token index.
///
/// Returns the index of the first token that is not a `VAR=value` assignment,
/// or `None` if all tokens are assignments.
fn find_command(tokens: &[&str]) -> Option<usize> {
    tokens.iter().position(|t| !ENV_VAR_RE.is_match(t))
}

/// Split a string on whitespace, respecting single and double quotes.
fn shell_split(s: &str) -> Vec<String> {
    let masked = mask_quotes(s);
    let masked_bytes = masked.as_bytes();
    let mut tokens = Vec::new();
    let mut start = None;

    for (i, &b) in masked_bytes.iter().enumerate() {
        if b == b' ' || b == b'\t' {
            if let Some(s_idx) = start {
                tokens.push(&s[s_idx..i]);
                start = None;
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s_idx) = start {
        tokens.push(&s[s_idx..]);
    }

    tokens.into_iter().map(String::from).collect()
}

/// Check whether a command is denied by the allowlist rules.
///
/// A command is denied if:
/// 1. It is not in `allow` or `pipeline` (and not the `build` tool).
/// 2. It is in `pipeline` but at pipe position 0.
/// 3. It is in `allow` but the specific subcommand is in `deny.<cmd>`.
///
/// The heredoc exception suppresses denial for commands reading from stdin.
/// Returns the denied command name if denied, `None` if allowed.
fn check_against_allowlist(
    name: &str,
    subcommand: Option<&str>,
    has_heredoc: bool,
    pipe_pos: usize,
    rules: &ResolvedCommands,
) -> Option<String> {
    // Heredoc exception: command is reading from stdin, not files.
    if has_heredoc {
        return None;
    }

    // Build tool is always allowed.
    if rules.build.as_deref() == Some(name) {
        return None;
    }

    // Check if command is in the unconditional allow list.
    if rules.allow.contains(name) {
        // Check subcommand deny: e.g., git is allowed but `git grep` is denied.
        if let Some(sub) = subcommand
            && let Some(denied_subs) = rules.deny.get(name)
            && denied_subs.contains(sub)
        {
            return Some(name.to_string());
        }
        return None;
    }

    // Check if command is in the pipeline list.
    if rules.pipeline.contains(name) {
        // Pipeline commands are only allowed mid-pipeline (not at position 0).
        if pipe_pos == 0 {
            return Some(name.to_string());
        }
        return None;
    }

    // Not in any allow list — denied.
    Some(name.to_string())
}

/// Check all commands in a shell command string against the allowlist rules.
///
/// Returns the denied command name for the first denied command, or `None`
/// if all commands are allowed.
pub fn check_command(cmd: &str, rules: &ResolvedCommands) -> Option<String> {
    let cmd_string = strip_heredoc_bodies(cmd);
    let cmd_string = strip_echo_separators(&cmd_string);

    let sequential = quote_aware_split(&cmd_string, &SEQ_SPLIT_RE);
    for seq in sequential {
        let stages = pipe_split(seq);
        for (pipe_pos, segment) in stages.iter().enumerate() {
            let segment = segment.trim();
            if segment.is_empty() {
                continue;
            }

            // Recursively check $(), <(), and `` substitutions.
            for m in SUBSHELL_RE.captures_iter(segment) {
                let inner = m
                    .get(1)
                    .or_else(|| m.get(2))
                    .or_else(|| m.get(3))
                    .map_or("", |g| g.as_str().trim());
                if let Some(reason) = check_command(inner, rules) {
                    return Some(reason);
                }
            }

            let tokens = shell_split(segment);
            let token_refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
            if token_refs.is_empty() {
                continue;
            }

            let Some(cmd_idx) = find_command(&token_refs) else {
                continue;
            };

            let name = std::path::Path::new(token_refs[cmd_idx])
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(token_refs[cmd_idx]);

            let rest = &token_refs[cmd_idx..];
            // Heredoc exception: only when `<<` is the first argument after
            // the command name. Quoted arguments (like sed patterns) are
            // invisible here because mask_quotes already collapsed them,
            // so `sed 's/foo/bar/' <<EOF` tokenizes as `["sed", "<<EOF"]`.
            // This prevents `rm -rf target/ <<EOF` from bypassing the
            // allowlist while preserving the `cat <<'EOF'` commit pattern.
            let has_heredoc = rest.get(1).is_some_and(|t| t.starts_with("<<"));
            let subcommand = if rest.len() > 1 { Some(rest[1]) } else { None };

            if let Some(denied) =
                check_against_allowlist(name, subcommand, has_heredoc, pipe_pos, rules)
            {
                return Some(denied);
            }
        }
    }

    None
}

/// Extract all command names from a shell command string.
///
/// Reuses the same parsing infrastructure as [`check_command`]: heredoc
/// stripping, echo separator removal, sequential/pipe splitting, subshell
/// recursion, env-var prefix skipping, and full-path stripping. Returns the
/// bare command names (e.g., `rm`, `cp`) found at each pipeline position.
///
/// Used by editing enforcement to decide whether a Bash tool call contains
/// only filesystem-manipulation commands.
#[must_use]
pub fn extract_command_names(cmd: &str) -> Vec<String> {
    let mut names = Vec::new();
    collect_command_names(cmd, &mut names);
    names
}

/// Recursive helper for [`extract_command_names`].
fn collect_command_names(cmd: &str, names: &mut Vec<String>) {
    let cmd_string = strip_heredoc_bodies(cmd);
    let cmd_string = strip_echo_separators(&cmd_string);

    let sequential = quote_aware_split(&cmd_string, &SEQ_SPLIT_RE);
    for seq in sequential {
        let stages = pipe_split(seq);
        for segment in &stages {
            let segment = segment.trim();
            if segment.is_empty() {
                continue;
            }

            // Recursively process $(), <(), and `` substitutions.
            for m in SUBSHELL_RE.captures_iter(segment) {
                let inner = m
                    .get(1)
                    .or_else(|| m.get(2))
                    .or_else(|| m.get(3))
                    .map_or("", |g| g.as_str().trim());
                collect_command_names(inner, names);
            }

            let tokens = shell_split(segment);
            let token_refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
            if token_refs.is_empty() {
                continue;
            }

            let Some(cmd_idx) = find_command(&token_refs) else {
                continue;
            };

            let name = std::path::Path::new(token_refs[cmd_idx])
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(token_refs[cmd_idx]);

            names.push(name.to_string());
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    /// Build a rule set matching the Python script's behavior for regression tests.
    ///
    /// The Python script used an allowlist model — this recreates those rules
    /// using the new `ResolvedCommands` allowlist structure.
    fn python_equivalent_rules() -> ResolvedCommands {
        ResolvedCommands {
            allow: HashSet::from([
                "make".into(),
                "git".into(),
                "gh".into(),
                "cp".into(),
                "mv".into(),
                "rm".into(),
                "mkdir".into(),
                "touch".into(),
                "chmod".into(),
                "sleep".into(),
                "cd".into(),
                "true".into(),
                "false".into(),
                "which".into(),
                "diff".into(),
            ]),
            pipeline: HashSet::from([
                "grep".into(),
                "egrep".into(),
                "fgrep".into(),
                "head".into(),
                "tail".into(),
                "sed".into(),
                "awk".into(),
                "sort".into(),
                "jq".into(),
                "wc".into(),
                "tr".into(),
                "cut".into(),
                "uniq".into(),
                "tee".into(),
            ]),
            deny: HashMap::from([(
                "git".into(),
                HashSet::from(["grep".into(), "ls-files".into(), "ls-tree".into()]),
            )]),
            build: Some("make".into()),
            client_enforcement_only: false,
        }
    }

    /// Minimal rule set for targeted tests.
    fn basic_rules() -> ResolvedCommands {
        ResolvedCommands {
            allow: HashSet::from([
                "make".into(),
                "git".into(),
                "gh".into(),
                "echo".into(),
                "diff".into(),
            ]),
            pipeline: HashSet::from(["grep".into(), "egrep".into(), "fgrep".into(), "sed".into()]),
            deny: HashMap::from([(
                "git".into(),
                HashSet::from(["grep".into(), "ls-files".into(), "ls-tree".into()]),
            )]),
            build: Some("make".into()),
            client_enforcement_only: false,
        }
    }

    // ── Deny basics ──────────────────────────────────────────────────

    #[test]
    fn deny_command_returns_name() {
        let rules = basic_rules();
        let result = check_command("cat file.txt", &rules);
        assert_eq!(result.as_deref(), Some("cat"));
    }

    #[test]
    fn allowed_command_returns_none() {
        let rules = basic_rules();
        assert!(check_command("make check", &rules).is_none());
    }

    #[test]
    fn pipeline_at_position_zero_denied() {
        let rules = basic_rules();
        assert!(check_command("grep pattern file", &rules).is_some());
    }

    #[test]
    fn pipeline_mid_pipeline_allowed() {
        let rules = basic_rules();
        assert!(check_command("echo foo | grep bar", &rules).is_none());
    }

    // ── Pipeline-safe ────────────────────────────────────────────────

    #[test]
    fn grep_standalone_denied() {
        let rules = basic_rules();
        assert!(check_command("grep pattern file", &rules).is_some());
    }

    #[test]
    fn grep_mid_pipeline_allowed() {
        let rules = basic_rules();
        assert!(check_command("echo foo | grep bar", &rules).is_none());
    }

    #[test]
    fn multi_stage_pipeline_allowed() {
        let rules = python_equivalent_rules();
        assert!(check_command("git log | sort", &rules).is_none());
    }

    #[test]
    fn denied_source_blocks_pipeline() {
        let rules = basic_rules();
        assert!(check_command("cat file | grep foo", &rules).is_some());
        assert!(check_command("ls | grep foo", &rules).is_some());
    }

    // ── Heredoc exception ────────────────────────────────────────────

    #[test]
    fn cat_heredoc_allowed() {
        let rules = basic_rules();
        assert!(check_command("cat <<EOF\nhello\nEOF", &rules).is_none());
    }

    #[test]
    fn cat_file_denied() {
        let rules = basic_rules();
        assert!(check_command("cat file.txt", &rules).is_some());
    }

    #[test]
    fn head_heredoc_quoted_marker_allowed() {
        // head is not in allow, but heredoc exception applies.
        let mut rules = ResolvedCommands::default();
        rules.allow.insert("git".to_string());
        assert!(check_command("head <<'MARKER'\nhello\nMARKER", &rules).is_none());
    }

    #[test]
    fn sed_heredoc_allowed() {
        let rules = basic_rules();
        assert!(check_command("sed 's/foo/bar/' <<EOF\nhello\nEOF", &rules).is_none());
    }

    #[test]
    fn heredoc_narrowing_unquoted_arg_before_heredoc() {
        // grep has an unquoted positional arg before <<, so the heredoc
        // exception does NOT fire. grep is in pipeline → denied at pos 0.
        let rules = basic_rules();
        assert!(check_command("grep pattern <<EOF\nhello\nEOF", &rules).is_some());
    }

    #[test]
    fn heredoc_narrowing_file_arg_before_heredoc() {
        // Adversarial: file operand before << prevents the exception.
        let rules = basic_rules();
        assert!(check_command("cat file.txt <<EOF\nhello\nEOF", &rules).is_some());
    }

    // ── Subshell recursion ───────────────────────────────────────────

    #[test]
    fn subshell_cat_denied() {
        let rules = basic_rules();
        assert!(check_command("echo $(cat file)", &rules).is_some());
    }

    #[test]
    fn subshell_grep_in_sequential_denied() {
        let rules = basic_rules();
        assert!(check_command("make test && $(grep -r pattern .)", &rules).is_some());
    }

    #[test]
    fn backtick_cat_denied() {
        let rules = basic_rules();
        assert!(check_command("`cat file`", &rules).is_some());
    }

    #[test]
    fn process_substitution_cat_denied() {
        let rules = basic_rules();
        assert!(check_command("diff <(cat file1) <(cat file2)", &rules).is_some());
    }

    // ── Quote-aware splitting ────────────────────────────────────────

    #[test]
    fn awk_pattern_not_split_on_and() {
        let rules = python_equivalent_rules();
        assert!(check_command("make test | awk '/a/ && /b/' | sort", &rules).is_none());
    }

    #[test]
    fn git_commit_message_not_split_on_semicolon() {
        let rules = basic_rules();
        assert!(check_command("git commit -m \"foo; bar\"", &rules).is_none());
    }

    #[test]
    fn git_commit_message_not_split_on_and() {
        let rules = basic_rules();
        assert!(check_command("git commit -m \"foo && bar\"", &rules).is_none());
    }

    #[test]
    fn pipe_inside_single_quotes_not_split() {
        let rules = python_equivalent_rules();
        assert!(check_command("make test | awk '/a|b/ {print}'", &rules).is_none());
    }

    // ── Subcommand deny ──────────────────────────────────────────────

    #[test]
    fn git_grep_denied() {
        let rules = basic_rules();
        assert!(check_command("git grep pattern", &rules).is_some());
    }

    #[test]
    fn git_commit_allowed() {
        let rules = basic_rules();
        assert!(check_command("git commit -m \"message\"", &rules).is_none());
    }

    #[test]
    fn git_ls_files_denied() {
        let rules = basic_rules();
        assert!(check_command("git ls-files", &rules).is_some());
    }

    #[test]
    fn cargo_not_allowed() {
        let rules = basic_rules();
        assert!(check_command("cargo test", &rules).is_some());
        assert!(check_command("cargo clippy", &rules).is_some());
    }

    // ── Env var prefix ───────────────────────────────────────────────

    #[test]
    fn env_var_prefix_allowed() {
        let rules = basic_rules();
        assert!(check_command("DEBUG=1 make test", &rules).is_none());
    }

    #[test]
    fn env_var_prefix_denied() {
        let rules = basic_rules();
        assert!(check_command("RUST_LOG=debug cargo test", &rules).is_some());
    }

    #[test]
    fn multiple_env_vars_denied() {
        let rules = basic_rules();
        assert!(check_command("A=1 B=2 cat file", &rules).is_some());
    }

    // ── Full path ────────────────────────────────────────────────────

    #[test]
    fn full_path_grep_denied() {
        let rules = basic_rules();
        assert!(check_command("/usr/bin/grep pattern", &rules).is_some());
    }

    #[test]
    fn full_path_cat_denied() {
        let rules = basic_rules();
        assert!(check_command("/bin/cat file.txt", &rules).is_some());
    }

    #[test]
    fn relative_path_denied() {
        let rules = basic_rules();
        assert!(check_command("./grep foo bar", &rules).is_some());
        assert!(check_command("../bin/grep foo bar", &rules).is_some());
    }

    // ── Regression tests (ported from Python) ────────────────────────

    mod regression {
        use super::*;

        // TestAllowed
        #[test]
        fn make() {
            let rules = python_equivalent_rules();
            assert!(check_command("make check", &rules).is_none());
        }

        #[test]
        fn git() {
            let rules = python_equivalent_rules();
            assert!(check_command("git status", &rules).is_none());
            assert!(check_command("git log --oneline", &rules).is_none());
            assert!(check_command("git commit -m 'fix bug'", &rules).is_none());
        }

        #[test]
        fn gh() {
            let rules = python_equivalent_rules();
            assert!(check_command("gh pr list", &rules).is_none());
            assert!(check_command("gh issue view 123", &rules).is_none());
        }

        #[test]
        fn sleep() {
            let rules = python_equivalent_rules();
            assert!(check_command("sleep 5", &rules).is_none());
        }

        #[test]
        fn cp_mv() {
            let rules = python_equivalent_rules();
            assert!(check_command("cp foo bar", &rules).is_none());
            assert!(check_command("mv foo bar", &rules).is_none());
        }

        #[test]
        fn env_prefix_allowed() {
            let rules = python_equivalent_rules();
            assert!(check_command("DEBUG=1 make check", &rules).is_none());
            assert!(check_command("RUST_LOG=debug make test", &rules).is_none());
        }

        // TestDenied
        #[test]
        fn cat_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("cat file.txt", &rules).is_some());
        }

        #[test]
        fn grep_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("grep foo bar.rs", &rules).is_some());
        }

        #[test]
        fn ls_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("ls -la", &rules).is_some());
        }

        #[test]
        fn find_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("find . -name '*.rs'", &rules).is_some());
        }

        #[test]
        fn cargo_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("cargo build", &rules).is_some());
            assert!(check_command("cargo test", &rules).is_some());
            assert!(check_command("cargo build 2>&1", &rules).is_some());
        }

        #[test]
        fn full_path_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("/usr/bin/grep foo bar", &rules).is_some());
            assert!(check_command("/bin/cat file.txt", &rules).is_some());
        }

        #[test]
        fn env_prefix_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("DEBUG=1 cargo test", &rules).is_some());
        }

        // TestGitDeniedSubcommands
        #[test]
        fn git_grep_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("git grep foo", &rules).is_some());
        }

        #[test]
        fn git_ls_files_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("git ls-files", &rules).is_some());
        }

        #[test]
        fn git_ls_tree_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("git ls-tree HEAD", &rules).is_some());
        }

        #[test]
        fn git_log_allowed() {
            let rules = python_equivalent_rules();
            assert!(check_command("git log --oneline", &rules).is_none());
        }

        #[test]
        fn git_diff_allowed() {
            let rules = python_equivalent_rules();
            assert!(check_command("git diff HEAD", &rules).is_none());
        }

        // TestPipeline
        #[test]
        fn grep_mid_pipeline() {
            let rules = python_equivalent_rules();
            assert!(check_command("gh pr list | grep foo", &rules).is_none());
        }

        #[test]
        fn head_mid_pipeline() {
            let rules = python_equivalent_rules();
            assert!(check_command("gh issue list | head -20", &rules).is_none());
        }

        #[test]
        fn tail_mid_pipeline() {
            let rules = python_equivalent_rules();
            assert!(check_command("git log --oneline | tail -5", &rules).is_none());
        }

        #[test]
        fn jq_mid_pipeline() {
            let rules = python_equivalent_rules();
            assert!(check_command("gh pr view --json title | jq .title", &rules).is_none());
        }

        #[test]
        fn wc_mid_pipeline() {
            let rules = python_equivalent_rules();
            assert!(check_command("gh issue list | wc -l", &rules).is_none());
        }

        #[test]
        fn multi_stage_pipeline() {
            let rules = python_equivalent_rules();
            assert!(check_command("gh pr list | grep open | head -5", &rules).is_none());
        }

        #[test]
        fn grep_standalone_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("grep foo bar.rs", &rules).is_some());
        }

        #[test]
        fn denied_source_blocks_pipeline() {
            let rules = python_equivalent_rules();
            assert!(check_command("cat file | grep foo", &rules).is_some());
            assert!(check_command("ls | grep foo", &rules).is_some());
        }

        // TestHeredoc
        #[test]
        fn git_commit_heredoc() {
            let rules = python_equivalent_rules();
            assert!(
                check_command("git commit -m \"$(cat <<'EOF'\nmessage\nEOF\n)\"", &rules).is_none()
            );
        }

        #[test]
        fn gh_pr_create_heredoc() {
            let rules = python_equivalent_rules();
            assert!(
                check_command(
                    "gh pr create --body \"$(cat <<'EOF'\nbody text\nEOF\n)\"",
                    &rules
                )
                .is_none()
            );
        }

        #[test]
        fn heredoc_body_with_semicolons() {
            let rules = python_equivalent_rules();
            let cmd = "git commit -m \"$(cat <<'EOF'\n\
                        feat: fix hook deny response\n\
                        \n\
                        - Fix display; add suppressOutput and systemMessage\n\
                        - Add chmod +x to script (missing execute bit)\n\
                        EOF\n\
                        )\"";
            assert!(check_command(cmd, &rules).is_none());
        }

        #[test]
        fn heredoc_body_with_parentheses() {
            let rules = python_equivalent_rules();
            let cmd = "git commit -m \"$(cat <<'EOF'\n\
                        fix(hook): missing execute bit was silently allowing blocked commands through)\n\
                        EOF\n\
                        )\"";
            assert!(check_command(cmd, &rules).is_none());
        }

        #[test]
        fn cat_file_still_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("cat file.txt", &rules).is_some());
        }

        // TestSubshell
        #[test]
        fn subshell_cat_standalone() {
            let rules = python_equivalent_rules();
            assert!(check_command("$(cat Makefile)", &rules).is_some());
        }

        #[test]
        fn subshell_cat_in_git_arg() {
            let rules = python_equivalent_rules();
            assert!(check_command("git commit -m \"$(cat file)\"", &rules).is_some());
        }

        #[test]
        fn backtick_grep() {
            let rules = python_equivalent_rules();
            assert!(check_command("`grep foo bar`", &rules).is_some());
        }

        #[test]
        fn backtick_cat() {
            let rules = python_equivalent_rules();
            assert!(check_command("make build `cat args.txt`", &rules).is_some());
        }

        // TestProcessSubstitution
        #[test]
        fn cat_inside_process_sub_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("git diff <(cat file1) <(cat file2)", &rules).is_some());
        }

        #[test]
        fn grep_inside_process_sub_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("git diff <(grep foo bar)", &rules).is_some());
        }

        #[test]
        fn git_show_inside_process_sub_allowed() {
            let rules = python_equivalent_rules();
            assert!(
                check_command(
                    "git diff <(git show HEAD:src/main.rs) <(git show HEAD~1:src/main.rs)",
                    &rules
                )
                .is_none()
            );
        }

        // TestSequential
        #[test]
        fn and_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("make build && cat file", &rules).is_some());
        }

        #[test]
        fn semicolon_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("git status; ls", &rules).is_some());
        }

        #[test]
        fn or_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("make check || cargo test", &rules).is_some());
        }

        #[test]
        fn both_allowed() {
            let rules = python_equivalent_rules();
            assert!(check_command("git fetch && make check", &rules).is_none());
        }

        // TestAdversarial
        #[test]
        fn env_var_before_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("FOO=0 grep foo bar", &rules).is_some());
        }

        #[test]
        fn multiple_env_vars_before_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("A=1 B=2 cat file", &rules).is_some());
        }

        #[test]
        fn env_var_before_denied_full_path() {
            let rules = python_equivalent_rules();
            assert!(check_command("PATH=/tmp /usr/bin/grep foo bar", &rules).is_some());
        }

        #[test]
        fn subshell_with_internal_spaces() {
            let rules = python_equivalent_rules();
            assert!(check_command("$( cat file )", &rules).is_some());
        }

        #[test]
        fn nested_subshell() {
            let rules = python_equivalent_rules();
            assert!(check_command("$(echo $(cat file))", &rules).is_some());
        }

        #[test]
        fn subshell_in_pipeline_position() {
            let rules = python_equivalent_rules();
            assert!(check_command("gh pr list | $(cat file)", &rules).is_some());
        }

        #[test]
        fn subshell_grep_in_pipeline() {
            let rules = python_equivalent_rules();
            assert!(check_command("gh pr list | $(grep foo bar.rs)", &rules).is_some());
        }

        #[test]
        fn backtick_in_git_arg() {
            let rules = python_equivalent_rules();
            assert!(check_command("git commit -m \"`cat file`\"", &rules).is_some());
        }

        #[test]
        fn semicolon_leading_subshell() {
            let rules = python_equivalent_rules();
            assert!(check_command("; $(head file)", &rules).is_some());
        }

        #[test]
        fn semicolon_then_cat_subshell() {
            let rules = python_equivalent_rules();
            assert!(check_command("make check; $(cat Makefile)", &rules).is_some());
        }

        #[test]
        fn semicolon_then_grep() {
            let rules = python_equivalent_rules();
            assert!(check_command("git status; grep foo bar", &rules).is_some());
        }

        #[test]
        fn herestring_with_subshell_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("head -5 <<< $(cat /etc/passwd)", &rules).is_some());
        }

        #[test]
        fn logical_or_both_checked() {
            let rules = python_equivalent_rules();
            assert!(check_command("make check || grep foo bar", &rules).is_some());
        }

        #[test]
        fn relative_path_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("./grep foo bar", &rules).is_some());
            assert!(check_command("../bin/grep foo bar", &rules).is_some());
        }

        #[test]
        fn git_diff_process_substitution_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("git diff <(cat file1) <(cat file2)", &rules).is_some());
        }

        // TestQuotedOperators
        #[test]
        fn awk_with_and_in_pattern() {
            let rules = python_equivalent_rules();
            assert!(check_command("make test | awk '/a/ && /b/' | sort", &rules).is_none());
        }

        #[test]
        fn pipe_inside_single_quotes() {
            let rules = python_equivalent_rules();
            assert!(check_command("make test | awk '/a|b/ {print}'", &rules).is_none());
        }

        #[test]
        fn semicolon_inside_single_quotes() {
            let rules = python_equivalent_rules();
            assert!(check_command("make ARGS='a;b;c' test", &rules).is_none());
        }

        #[test]
        fn and_inside_double_quotes() {
            let rules = python_equivalent_rules();
            assert!(check_command("git commit -m \"foo && bar\"", &rules).is_none());
        }

        #[test]
        fn pipe_inside_double_quotes() {
            let rules = python_equivalent_rules();
            assert!(check_command("git commit -m \"a | b\"", &rules).is_none());
        }

        #[test]
        fn semicolon_inside_double_quotes() {
            let rules = python_equivalent_rules();
            assert!(check_command("git commit -m \"a; b\"", &rules).is_none());
        }

        #[test]
        fn unquoted_operators_still_split() {
            let rules = python_equivalent_rules();
            assert!(check_command("make build && cat file", &rules).is_some());
            assert!(check_command("make build; ls .", &rules).is_some());
            assert!(check_command("cat file | grep foo", &rules).is_some());
        }
    }

    // ── mask_quotes unit tests ───────────────────────────────────────

    #[test]
    fn mask_quotes_single() {
        let result = mask_quotes("echo 'foo && bar'");
        assert!(!result.contains("foo"));
        assert_eq!(result.len(), "echo 'foo && bar'".len());
    }

    #[test]
    fn mask_quotes_double() {
        let result = mask_quotes("echo \"foo | bar\"");
        assert!(!result.contains("foo"));
        assert_eq!(result.len(), "echo \"foo | bar\"".len());
    }

    #[test]
    fn mask_quotes_preserves_unquoted() {
        let result = mask_quotes("echo hello && world");
        assert!(result.contains("echo"));
        assert!(result.contains("hello"));
        assert!(result.contains("&&"));
        assert!(result.contains("world"));
    }

    // ── strip_heredoc_bodies tests ───────────────────────────────────

    #[test]
    fn strip_heredoc_removes_body() {
        let input = "cat <<EOF\nhello world\nfoo bar\nEOF";
        let result = strip_heredoc_bodies(input);
        assert!(!result.contains("hello world"));
        assert!(!result.contains("foo bar"));
        assert!(result.contains("cat <<EOF"));
        assert!(result.contains("EOF"));
    }

    #[test]
    fn strip_heredoc_preserves_non_heredoc() {
        let input = "make build && git status";
        let result = strip_heredoc_bodies(input);
        assert_eq!(result, input);
    }

    // ── find_command tests ───────────────────────────────────────────

    #[test]
    fn find_command_no_env_vars() {
        assert_eq!(find_command(&["make", "test"]), Some(0));
    }

    #[test]
    fn find_command_skips_env_vars() {
        assert_eq!(find_command(&["DEBUG=1", "make", "test"]), Some(1));
        assert_eq!(find_command(&["A=1", "B=2", "cat", "file"]), Some(2));
    }

    #[test]
    fn find_command_all_env_vars() {
        assert_eq!(find_command(&["A=1", "B=2"]), None);
    }

    // ── pipe_split tests ─────────────────────────────────────────────

    #[test]
    fn pipe_split_basic() {
        let parts = pipe_split("echo foo | grep bar");
        assert_eq!(parts, vec!["echo foo", "grep bar"]);
    }

    #[test]
    fn pipe_split_preserves_or() {
        let parts = pipe_split("make check || cargo test");
        assert_eq!(parts, vec!["make check || cargo test"]);
    }

    #[test]
    fn pipe_split_multi_stage() {
        let parts = pipe_split("a | b | c");
        assert_eq!(parts, vec!["a", "b", "c"]);
    }

    #[test]
    fn pipe_split_quoted_pipe() {
        let parts = pipe_split("git commit -m \"a | b\"");
        assert_eq!(parts, vec!["git commit -m \"a | b\""]);
    }

    // ── extract_command_names tests ─────────────────────────────────

    #[test]
    fn extract_names_simple() {
        let names = extract_command_names("rm -rf target/");
        assert_eq!(names, vec!["rm"]);
    }

    #[test]
    fn extract_names_chained() {
        let names = extract_command_names("mkdir -p src/new && touch src/new/mod.rs");
        assert_eq!(names, vec!["mkdir", "touch"]);
    }

    #[test]
    fn extract_names_pipeline() {
        let names = extract_command_names("find . -name '*.rs' | grep test");
        assert_eq!(names, vec!["find", "grep"]);
    }

    #[test]
    fn extract_names_full_path() {
        let names = extract_command_names("/usr/bin/cp a b");
        assert_eq!(names, vec!["cp"]);
    }

    #[test]
    fn extract_names_env_prefix() {
        let names = extract_command_names("LANG=C rm foo.rs");
        assert_eq!(names, vec!["rm"]);
    }

    #[test]
    fn extract_names_subshell() {
        let names = extract_command_names("rm $(cat files.txt)");
        assert_eq!(names, vec!["cat", "rm"]);
    }

    #[test]
    fn extract_names_empty() {
        let names = extract_command_names("");
        assert!(names.is_empty());
    }
}

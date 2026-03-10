// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Diagnostic noise filter for rust-analyzer.
//!
//! Strips well-known boilerplate from clippy and rustc diagnostics:
//! - `"for further information visit https://..."` URL lines
//! - Lint attribution lines (`` `#[warn(...)]` on by default ``, etc.)

use super::{DiagnosticCode, DiagnosticFilter};

/// Diagnostic filter for rust-analyzer.
pub struct RustAnalyzerFilter;

impl RustAnalyzerFilter {
    /// Known version range: tested against rust-analyzer 1.x.
    /// Pass through unrecognized major versions without filtering.
    fn is_known_version(version: Option<&str>) -> bool {
        version.is_some_and(|v| v.starts_with("1."))
    }
}

impl DiagnosticFilter for RustAnalyzerFilter {
    fn filter_message(
        &self,
        _server: &str,
        version: Option<&str>,
        _source: Option<&str>,
        _code: Option<&DiagnosticCode>,
        _severity: u8,
        _language_id: &str,
        message: &str,
    ) -> String {
        // Version safety: pass through unchanged for unrecognized versions
        if !Self::is_known_version(version) {
            return message.to_string();
        }

        message
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                // Clippy "for further information visit ..." URL lines.
                if trimmed.starts_with("for further information visit") {
                    return false;
                }
                // Rustc/clippy lint attribution: "`#[warn(...)]` on by default" etc.
                if trimmed.starts_with("`#[")
                    && (trimmed.contains("on by default")
                        || trimmed.contains("implied by")
                        || trimmed.contains("to override"))
                {
                    return false;
                }
                // Standalone `-W` flag attribution: "`-W clippy::...` implied by ..."
                if trimmed.starts_with("`-W ") && trimmed.contains("implied by") {
                    return false;
                }
                // Standalone override instruction: "to override `-W ...` add `#[allow(...)]`"
                if trimmed.starts_with("to override")
                    && (trimmed.contains("`#[allow(") || trimmed.contains("`#[warn("))
                {
                    return false;
                }
                true
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string()
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    const KNOWN_VERSION: Option<&str> = Some("1.92.0");

    fn filter(version: Option<&str>, message: &str) -> String {
        RustAnalyzerFilter.filter_message(
            "rust-analyzer",
            version,
            Some("clippy"),
            None,
            crate::filter::SEVERITY_WARNING,
            "rust",
            message,
        )
    }

    #[test]
    fn strips_clippy_url() {
        let msg = "unused variable `x`\nfor further information visit https://rust-lang.github.io/rust-clippy/master/index.html#unused_variable";
        assert_eq!(filter(KNOWN_VERSION, msg), "unused variable `x`");
    }

    #[test]
    fn strips_lint_attribution_on_by_default() {
        let msg = "unused variable `x`\n`#[warn(unused_variables)]` on by default";
        assert_eq!(filter(KNOWN_VERSION, msg), "unused variable `x`");
    }

    #[test]
    fn strips_lint_attribution_implied_by() {
        let msg =
            "unused variable `x`\n`#[warn(clippy::pedantic)]` implied by `#[warn(clippy::all)]`";
        assert_eq!(filter(KNOWN_VERSION, msg), "unused variable `x`");
    }

    #[test]
    fn strips_lint_attribution_to_override() {
        let msg = "unused variable `x`\n`#[allow(unused)]` to override `#[warn(unused_variables)]`";
        assert_eq!(filter(KNOWN_VERSION, msg), "unused variable `x`");
    }

    #[test]
    fn strips_multiple_noise_lines() {
        let msg = "needless return\nfor further information visit https://example.com\n`#[warn(clippy::needless_return)]` on by default";
        assert_eq!(filter(KNOWN_VERSION, msg), "needless return");
    }

    #[test]
    fn preserves_clean_message() {
        let msg = "expected `usize`, found `&str`";
        assert_eq!(filter(KNOWN_VERSION, msg), msg);
    }

    #[test]
    fn preserves_multiline_clean_message() {
        let msg = "mismatched types\nexpected `usize`\n   found `&str`";
        assert_eq!(filter(KNOWN_VERSION, msg), msg);
    }

    #[test]
    fn passthrough_for_unknown_version() {
        let msg = "unused variable `x`\nfor further information visit https://example.com";
        assert_eq!(filter(Some("2.0.0"), msg), msg);
    }

    #[test]
    fn passthrough_for_no_version() {
        let msg = "unused variable `x`\nfor further information visit https://example.com";
        assert_eq!(filter(None, msg), msg);
    }

    #[test]
    fn drops_standalone_w_flag_implied_by() {
        let msg = "`-W clippy::doc-markdown` implied by `-W clippy::pedantic`";
        assert_eq!(filter(KNOWN_VERSION, msg), "");
    }

    #[test]
    fn drops_standalone_to_override_with_allow() {
        let msg = "to override `-W clippy::pedantic` add `#[allow(clippy::doc_markdown)]`";
        assert_eq!(filter(KNOWN_VERSION, msg), "");
    }

    #[test]
    fn drops_standalone_to_override_with_warn() {
        let msg = "to override `-D warnings` add `#[warn(clippy::doc_markdown)]`";
        assert_eq!(filter(KNOWN_VERSION, msg), "");
    }

    #[test]
    fn known_version_check() {
        assert!(RustAnalyzerFilter::is_known_version(Some("1.92.0")));
        assert!(RustAnalyzerFilter::is_known_version(Some("1.0.0")));
        assert!(!RustAnalyzerFilter::is_known_version(Some("2.0.0")));
        assert!(!RustAnalyzerFilter::is_known_version(None));
    }
}

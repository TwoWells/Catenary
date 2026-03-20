// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Diagnostic noise filtering for LSP server output.
//!
//! Each LSP server may attach noise to diagnostic messages (reference URLs,
//! lint attribution boilerplate, etc.) that wastes tokens when delivered to
//! AI agents. This module provides a [`DiagnosticFilter`] trait with
//! per-server implementations that rewrite or drop noisy messages.
//!
//! Implementations **must** default to pass-through for unrecognized server
//! versions — we are writing regexes against output format we don't own.

mod rust_analyzer;

/// LSP severity constants (1=Error through 4=Hint).
pub const SEVERITY_ERROR: u8 = 1;
/// Warning severity.
pub const SEVERITY_WARNING: u8 = 2;
/// Information severity.
pub const SEVERITY_INFORMATION: u8 = 3;
/// Hint severity.
pub const SEVERITY_HINT: u8 = 4;

/// LSP diagnostic code, which can be a number or a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticCode {
    /// Numeric diagnostic code (e.g., TypeScript `6133`).
    Number(i64),
    /// String diagnostic code (e.g., clippy `"needless_return"`).
    Text(String),
}

impl DiagnosticCode {
    /// Converts from a JSON diagnostic code value.
    #[must_use]
    pub fn from_value(code: &serde_json::Value) -> Self {
        code.as_i64().map_or_else(
            || {
                code.as_str().map_or_else(
                    || Self::Text(code.to_string()),
                    |s| Self::Text(s.to_string()),
                )
            },
            Self::Number,
        )
    }
}

/// Trait for filtering noise from LSP diagnostic messages.
///
/// Implementations are per-server (keyed by the server command name, e.g.,
/// `"rust-analyzer"`). The default implementation passes messages through
/// unchanged.
///
/// # Return value
///
/// - Non-empty string: deliver this message (original or rewritten).
/// - Empty string: drop the diagnostic entirely.
#[allow(
    clippy::too_many_arguments,
    reason = "diagnostic context requires all fields"
)]
pub trait DiagnosticFilter: Send + Sync {
    /// Filters noise from a diagnostic message.
    fn filter_message(
        &self,
        server: &str,
        version: Option<&str>,
        source: Option<&str>,
        code: Option<&DiagnosticCode>,
        severity: u8,
        language_id: &str,
        message: &str,
    ) -> String;
}

/// Default filter that passes all messages through unchanged.
struct DefaultFilter;

impl DiagnosticFilter for DefaultFilter {
    fn filter_message(
        &self,
        _server: &str,
        _version: Option<&str>,
        _source: Option<&str>,
        _code: Option<&DiagnosticCode>,
        _severity: u8,
        _language_id: &str,
        message: &str,
    ) -> String {
        message.to_string()
    }
}

/// Returns the appropriate diagnostic filter for a server command.
///
/// Matches on the command name (e.g., `"rust-analyzer"`) because different
/// servers for the same language (pylsp vs pyright vs ruff) need different
/// filters.
#[must_use]
pub fn get_filter(server_command: &str) -> &'static dyn DiagnosticFilter {
    static DEFAULT: DefaultFilter = DefaultFilter;
    static RUST_ANALYZER: rust_analyzer::RustAnalyzerFilter = rust_analyzer::RustAnalyzerFilter;

    match server_command {
        "rust-analyzer" => &RUST_ANALYZER,
        _ => &DEFAULT,
    }
}

/// Parses a severity string from config into a `u8` (LSP severity encoding).
///
/// Returns `None` for unrecognized values (caller should treat as "no threshold").
#[must_use]
pub fn parse_severity(s: &str) -> Option<u8> {
    match s.to_ascii_lowercase().as_str() {
        "error" => Some(SEVERITY_ERROR),
        "warning" => Some(SEVERITY_WARNING),
        "information" | "info" => Some(SEVERITY_INFORMATION),
        "hint" => Some(SEVERITY_HINT),
        _ => None,
    }
}

/// Returns `true` if the diagnostic severity meets or exceeds the threshold.
///
/// LSP severity is inverted: 1 = Error (most severe), 4 = Hint (least).
/// A diagnostic passes if its severity value is ≤ the threshold value.
#[must_use]
pub const fn severity_passes(severity: u8, threshold: u8) -> bool {
    severity_rank(severity) <= severity_rank(threshold)
}

/// Maps severity to a numeric rank for comparison.
/// Lower rank = more severe (Error=1, Warning=2, Info=3, Hint=4, unknown=5).
const fn severity_rank(s: u8) -> u8 {
    match s {
        1..=4 => s,
        _ => 5,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn default_filter_passes_through() {
        let filter = DefaultFilter;
        let result = filter.filter_message(
            "some-server",
            None,
            None,
            None,
            SEVERITY_WARNING,
            "rust",
            "unused variable `x`",
        );
        assert_eq!(result, "unused variable `x`");
    }

    #[test]
    fn get_filter_returns_default_for_unknown() {
        let filter = get_filter("unknown-server");
        let result = filter.filter_message(
            "unknown-server",
            None,
            None,
            None,
            SEVERITY_ERROR,
            "python",
            "syntax error",
        );
        assert_eq!(result, "syntax error");
    }

    #[test]
    fn get_filter_returns_rust_analyzer() {
        let filter = get_filter("rust-analyzer");
        // Should strip the URL line
        let message = "unused variable `x`\nfor further information visit https://example.com";
        let result = filter.filter_message(
            "rust-analyzer",
            Some("1.92.0"),
            Some("clippy"),
            None,
            SEVERITY_WARNING,
            "rust",
            message,
        );
        assert_eq!(result, "unused variable `x`");
    }

    #[test]
    fn parse_severity_valid() {
        assert_eq!(parse_severity("error"), Some(SEVERITY_ERROR));
        assert_eq!(parse_severity("Warning"), Some(SEVERITY_WARNING));
        assert_eq!(parse_severity("information"), Some(SEVERITY_INFORMATION));
        assert_eq!(parse_severity("info"), Some(SEVERITY_INFORMATION));
        assert_eq!(parse_severity("hint"), Some(SEVERITY_HINT));
    }

    #[test]
    fn parse_severity_invalid() {
        assert_eq!(parse_severity("bogus"), None);
    }

    #[test]
    fn severity_passes_threshold() {
        // Error passes warning threshold
        assert!(severity_passes(SEVERITY_ERROR, SEVERITY_WARNING));
        // Warning passes warning threshold
        assert!(severity_passes(SEVERITY_WARNING, SEVERITY_WARNING));
        // Hint does not pass warning threshold
        assert!(!severity_passes(SEVERITY_HINT, SEVERITY_WARNING));
        // Info does not pass warning threshold
        assert!(!severity_passes(SEVERITY_INFORMATION, SEVERITY_WARNING));
    }

    #[test]
    fn diagnostic_code_from_value() {
        use serde_json::json;

        assert_eq!(
            DiagnosticCode::from_value(&json!(6133)),
            DiagnosticCode::Number(6133)
        );
        assert_eq!(
            DiagnosticCode::from_value(&json!("needless_return")),
            DiagnosticCode::Text("needless_return".to_string())
        );
    }
}

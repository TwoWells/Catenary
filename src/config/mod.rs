// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Configuration handling for language servers and session settings.

mod language;
mod parse;
mod server;
mod validate;

use std::collections::HashMap;

use anyhow::Result;
use serde::Deserialize;

pub use language::{LanguageConfig, ServerBinding};
pub use parse::{SERVER_DEF_KEYS, config_sources};
pub use server::ServerDef;

/// Notification delivery configuration.
///
/// Controls which tracing events are promoted to user-facing notifications
/// via `systemMessage`. Events below the threshold are silently dropped by
/// the notification queue sink.
///
/// # Examples
///
/// ```toml
/// [notifications]
/// threshold = "warn"
/// ```
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationConfig {
    /// Minimum severity for notification delivery.
    pub threshold: SeverityConfig,
}

/// Severity level for notification threshold configuration.
///
/// Deserialized from lowercase TOML strings (`"debug"`, `"info"`, `"warn"`,
/// `"error"`). Defaults to `Warn`.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SeverityConfig {
    /// Include debug-level events (most verbose).
    Debug,
    /// Include info-level and above.
    Info,
    /// Include warn-level and above (default).
    #[default]
    Warn,
    /// Only error-level events.
    Error,
}

impl From<SeverityConfig> for crate::logging::Severity {
    fn from(sc: SeverityConfig) -> Self {
        match sc {
            SeverityConfig::Debug => Self::Debug,
            SeverityConfig::Info => Self::Info,
            SeverityConfig::Warn => Self::Warn,
            SeverityConfig::Error => Self::Error,
        }
    }
}

/// Overall configuration for Catenary.
#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    /// Log retention in days (default: 7).
    /// 0 = no persistent logging (cleanup on exit).
    /// -1 = retain logs forever.
    #[serde(default = "default_log_retention_days")]
    pub log_retention_days: i64,

    /// Language definitions keyed by language ID (e.g., "rust", "python").
    #[serde(default)]
    pub language: HashMap<String, LanguageConfig>,

    /// Server definitions keyed by server name.
    #[serde(default)]
    pub server: HashMap<String, ServerDef>,

    /// Notification delivery configuration.
    ///
    /// `None` when no source specified `[notifications]`. Use
    /// `unwrap_or_default()` at consumption sites to get the default
    /// threshold (`warn`). Kept as `Option` so layered merge can
    /// distinguish "absent" from "explicitly set to default".
    #[serde(default)]
    pub notifications: Option<NotificationConfig>,

    /// Icon theme configuration.
    ///
    /// `None` when no source specified `[icons]`. Absent sections fall
    /// through to the earlier config layer.
    #[serde(default)]
    pub icons: Option<IconConfig>,

    /// TUI configuration.
    ///
    /// `None` when no source specified `[tui]`. Absent sections fall
    /// through to the earlier config layer.
    #[serde(default)]
    pub tui: Option<TuiConfig>,
}

/// Icon preset selecting a base set of icons.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "lowercase")]
pub enum IconPreset {
    /// Safe Unicode symbols that render on any terminal font.
    #[default]
    Unicode,
    /// Nerd Font glyphs (requires a patched font).
    Nerd,
    /// Emoji icons (Unicode 17.0, requires emoji-capable font).
    Emoji,
}

/// Icon theme configuration.
///
/// Set `preset` to choose a base icon set, then override individual icons
/// as needed. Each override replaces the preset default for that slot.
///
/// # Examples
///
/// ```toml
/// [icons]
/// preset = "nerd"
/// ```
///
#[derive(Debug, Deserialize, Clone, Default)]
pub struct IconConfig {
    /// Base icon preset (default: `unicode`).
    #[serde(default)]
    pub preset: IconPreset,
    /// Diagnostic error icon.
    pub diag_error: Option<String>,
    /// Diagnostic warning icon.
    pub diag_warn: Option<String>,
    /// Diagnostic info icon.
    pub diag_info: Option<String>,
    /// Diagnostic ok (clean) icon.
    pub diag_ok: Option<String>,
    /// Search tool icon.
    pub tool_search: Option<String>,
    /// Glob tool icon.
    pub tool_glob: Option<String>,
    /// Default tool icon (fallback).
    pub tool_default: Option<String>,
    /// Workspace expanded icon.
    pub workspace_open: Option<String>,
    /// Workspace collapsed icon.
    pub workspace_closed: Option<String>,
    /// Pinned panel icon.
    pub pinned: Option<String>,
    /// Progress spinner frames (animated).
    pub progress: Option<String>,
    /// Session started event icon.
    pub session_started: Option<String>,
    /// Session shutdown event icon.
    pub session_shutdown: Option<String>,
    /// Server state change event icon.
    pub server_state: Option<String>,
    /// Sed tool icon.
    pub tool_sed: Option<String>,
    /// Language server active icon.
    pub ls_active: Option<String>,
    /// Language server inactive icon.
    pub ls_inactive: Option<String>,
    /// Protocol success icon.
    pub proto_ok: Option<String>,
    /// Protocol error icon.
    pub proto_error: Option<String>,
    /// Request cancelled icon.
    pub cancelled: Option<String>,
    /// Server log info icon (collapsed `window/logMessage` runs at info level).
    pub log_info: Option<String>,
    /// Spinner grow phase frames (plays once at start).
    pub spinner_grow: Option<Vec<String>>,
    /// Spinner cycle phase frames (loops during progress).
    pub spinner_cycle: Option<Vec<String>>,
    /// Spinner done frame (shown on progress end).
    pub spinner_done: Option<String>,
}

/// TUI configuration options.
///
/// Controls the interactive monitor's layout and behavior.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct TuiConfig {
    /// Automatically add new sessions to the grid (default: true).
    pub auto_add_sessions: bool,

    /// Preferred width of the Sessions tree as a fraction of the terminal
    /// (default: 0.25).
    pub sessions_width: f64,

    /// Whether mouse hover changes focus (default: false).
    pub focus_follows_mouse: bool,

    /// Capture full tool output in `ToolResult` events for TUI detail
    /// expansion (default: false). Increases database size.
    pub capture_tool_output: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            auto_add_sessions: true,
            sessions_width: 0.25,
            focus_follows_mouse: false,
            capture_tool_output: false,
        }
    }
}

pub(crate) const fn default_log_retention_days() -> i64 {
    7
}

impl Config {
    /// Load configuration from standard paths or a specific file.
    ///
    /// Sources are loaded in order, with later sources overriding earlier ones:
    /// 1. User config (`~/.config/catenary/config.toml`)
    /// 2. Project-local config (`.catenary.toml`, searching upward from cwd)
    /// 3. Explicit file (if provided)
    /// 4. Environment variable overrides
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A configuration file exists but cannot be read or parsed.
    /// - A file uses the deprecated `[server.*]` key without `[language.*]`.
    /// - A `[language.*]` entry uses the removed `inherit` field.
    /// - A concrete language entry has no `servers` list.
    pub fn load() -> Result<Self> {
        parse::load()
    }

    /// Parse and validate configuration without side effects.
    ///
    /// Reads config sources, parses TOML, and runs validation. Returns
    /// `Ok(())` if the config is valid, or an error describing what's wrong.
    /// Does not spawn servers, scan the filesystem, or access the database.
    ///
    /// # Errors
    ///
    /// Returns an error if any config source cannot be read or parsed, or
    /// if validation finds issues (missing servers, broken inherits, etc.).
    pub fn check() -> Result<()> {
        let _ = Self::load()?;
        Ok(())
    }

    /// Load configuration from an explicit list of file paths.
    ///
    /// Sources are merged in order (later overrides earlier). Environment
    /// variable overrides and validation are applied after merging.
    #[cfg(test)]
    fn load_from_sources(sources: &[std::path::PathBuf]) -> Result<Self> {
        parse::load_from_sources(sources)
    }

    /// Merge another config layer into this one. Later values override.
    fn merge(&mut self, other: Self) {
        parse::merge(self, other);
    }

    /// Apply environment variable overrides for supported keys.
    fn apply_env_overrides(&mut self) {
        parse::apply_env_overrides(self);
    }

    /// Validate the merged config, returning all errors found.
    ///
    /// Returns an empty vec when the config is valid.
    #[must_use]
    pub fn validate(&self) -> Vec<String> {
        validate::validate(self)
    }

    /// Look up the configuration for a language key.
    #[must_use]
    pub fn resolve_language(&self, key: &str) -> Option<&LanguageConfig> {
        self.language.get(key)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            log_retention_days: default_log_retention_days(),
            language: HashMap::new(),
            server: HashMap::new(),
            notifications: None,
            icons: None,
            tui: None,
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_config_load_local() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust-analyzer]
command = "rust-analyzer-local"

[language.rust]
servers = ["rust-analyzer"]
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        assert_eq!(
            config
                .language
                .get("rust")
                .expect("rust language config")
                .servers,
            vec![ServerBinding::new("rust-analyzer")],
        );

        Ok(())
    }

    #[test]
    fn test_old_server_key_hard_error() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust]
command = "rust-analyzer"
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("deprecated"),
            "error should mention deprecated: {err}",
        );
    }

    #[test]
    fn test_server_def_parsing() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"
args = ["--log-level", "info"]

[server.clangd]
command = "clangd"
args = ["--background-index"]
settings = { checkOnSave = true }

[language.rust]
servers = ["rust-analyzer"]

[language.c]
servers = ["clangd"]
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        assert!(config.language.contains_key("rust"));
        assert_eq!(config.server.len(), 2);

        let ra = config
            .server
            .get("rust-analyzer")
            .expect("rust-analyzer server def");
        assert_eq!(ra.command, "rust-analyzer");
        assert_eq!(ra.args, vec!["--log-level", "info"]);

        let clangd = config.server.get("clangd").expect("clangd server def");
        assert_eq!(clangd.command, "clangd");
        assert_eq!(clangd.args, vec!["--background-index"]);
        assert!(clangd.settings.is_some());

        Ok(())
    }

    #[test]
    fn test_both_server_and_language_valid() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"

[language.rust]
servers = ["rust-analyzer"]
"#,
        )?;

        // This should succeed — new format with both sections
        let config = Config::load_from_sources(&[config_path])?;
        assert!(config.server.contains_key("rust-analyzer"));
        assert!(config.language.contains_key("rust"));

        Ok(())
    }

    #[test]
    fn test_server_def_merge() -> anyhow::Result<()> {
        let dir = tempdir()?;

        let source1 = dir.path().join("source1.toml");
        fs::write(
            &source1,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"

[server.clangd]
command = "clangd"
args = ["--background-index"]

[language.rust]
servers = ["rust-analyzer"]
"#,
        )?;

        let source2 = dir.path().join("source2.toml");
        fs::write(
            &source2,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"

[server.clangd]
command = "clangd"
args = ["--background-index", "--clang-tidy"]
settings = { checkOnSave = true }

[language.rust]
servers = ["rust-analyzer"]
"#,
        )?;

        let config = Config::load_from_sources(&[source1, source2])?;

        let clangd = config.server.get("clangd").expect("clangd server def");
        assert_eq!(clangd.args, vec!["--background-index", "--clang-tidy"]);
        assert!(clangd.settings.is_some());

        Ok(())
    }

    #[test]
    fn test_server_def_validation() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"

[server.bad-server]
command = ""

[language.rust]
servers = ["rust-analyzer"]
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("empty") && err.contains("command"),
            "error should mention empty command: {err}",
        );
    }

    #[test]
    fn test_inherit_field_rejected() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.tsserver]
command = "typescript-language-server"

[language.typescript]
servers = ["tsserver"]

[language.typescriptreact]
inherit = "typescript"
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("inherit") && err.contains("removed"),
            "error should mention removed inherit field: {err}",
        );
    }

    #[test]
    fn test_concrete_without_servers_rejected() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r"
[language.rust]
diagnostics = false
",
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("servers"),
            "error should mention servers: {err}",
        );
    }

    #[test]
    fn test_resolve_language_direct() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.tsserver]
command = "typescript-language-server"
args = ["--stdio"]

[language.typescript]
servers = ["tsserver"]
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        let resolved = config
            .resolve_language("typescript")
            .expect("should resolve");
        assert_eq!(resolved.servers, vec![ServerBinding::new("tsserver")]);

        // Unconfigured language returns None
        assert!(config.resolve_language("typescriptreact").is_none());

        Ok(())
    }

    #[test]
    fn test_empty_config() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "")?;

        let config = Config::load_from_sources(&[config_path])?;
        assert_eq!(config.log_retention_days, 7);
        assert!(config.language.is_empty());
        assert!(config.server.is_empty());

        Ok(())
    }

    #[test]
    fn test_merge_later_source_overrides() -> anyhow::Result<()> {
        let dir = tempdir()?;

        let local_config_path = dir.path().join(".catenary.toml");
        fs::write(
            &local_config_path,
            r#"
log_retention_days = 14

[server.rust-analyzer]
command = "rust-analyzer-local"

[language.rust]
servers = ["rust-analyzer"]
"#,
        )?;

        let explicit_path = dir.path().join("explicit.toml");
        fs::write(
            &explicit_path,
            r"
log_retention_days = 30
",
        )?;

        let config = Config::load_from_sources(&[local_config_path, explicit_path])?;

        assert_eq!(config.log_retention_days, 30);
        assert!(config.language.contains_key("rust"));

        Ok(())
    }

    #[test]
    fn test_new_format_roundtrip() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"
args = ["--log-level", "info"]
min_severity = "warning"

[server.clangd]
command = "clangd"
args = ["--background-index"]

[language.rust]
servers = ["rust-analyzer"]

[language.c]
servers = ["clangd"]

[language.cpp]
servers = ["clangd"]
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        // Server defs
        let ra = config
            .server
            .get("rust-analyzer")
            .expect("rust-analyzer server def");
        assert_eq!(ra.command, "rust-analyzer");
        assert_eq!(ra.args, vec!["--log-level", "info"]);
        assert_eq!(ra.min_severity.as_deref(), Some("warning"));

        // Language entries
        let rust = config.language.get("rust").expect("rust config");
        assert_eq!(rust.servers, vec![ServerBinding::new("rust-analyzer")]);

        let c = config.language.get("c").expect("c config");
        assert_eq!(c.servers, vec![ServerBinding::new("clangd")]);

        let cpp = config.language.get("cpp").expect("cpp config");
        assert_eq!(cpp.servers, vec![ServerBinding::new("clangd")]);

        Ok(())
    }

    #[test]
    fn test_inline_command_hard_error() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[language.rust]
command = "rust-analyzer"
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("command") && err.contains("[server.*]"),
            "error should mention server definition migration: {err}",
        );
    }

    #[test]
    fn test_undefined_server_ref() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[language.rust]
servers = ["nonexistent-server"]
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("nonexistent-server"),
            "error should mention the undefined server: {err}",
        );
    }

    #[test]
    fn test_concrete_empty_servers() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r"
[language.rust]
servers = []
",
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("servers"),
            "error should mention servers: {err}",
        );
    }

    #[test]
    fn test_resolve_language_borrows() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.tsserver]
command = "typescript-language-server"
min_severity = "warning"

[language.typescript]
servers = ["tsserver"]
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        // Verify the returned config borrows from the map
        let resolved = config
            .resolve_language("typescript")
            .expect("should resolve");
        assert_eq!(resolved.servers, vec![ServerBinding::new("tsserver")]);

        let server = config.server.get("tsserver").expect("tsserver def");
        assert_eq!(server.min_severity.as_deref(), Some("warning"));

        Ok(())
    }

    #[test]
    fn test_parse_server_specs_single() {
        let results = parse::parse_server_specs("rust:rust-analyzer --log-level info");
        assert_eq!(results.len(), 1);

        let (lang, server_def, lang_config) = &results[0];
        assert_eq!(lang, "rust");
        assert_eq!(server_def.command, "rust-analyzer");
        assert_eq!(server_def.args, vec!["--log-level", "info"]);
        assert_eq!(lang_config.servers, vec![ServerBinding::new("rust")]);
    }

    #[test]
    fn test_parse_server_specs_multiple() {
        let results =
            parse::parse_server_specs("rust:rust-analyzer;python:pyright --stdio;c:clangd");
        assert_eq!(results.len(), 3);

        assert_eq!(results[0].0, "rust");
        assert_eq!(results[0].1.command, "rust-analyzer");
        assert!(results[0].1.args.is_empty());

        assert_eq!(results[1].0, "python");
        assert_eq!(results[1].1.command, "pyright");
        assert_eq!(results[1].1.args, vec!["--stdio"]);

        assert_eq!(results[2].0, "c");
        assert_eq!(results[2].1.command, "clangd");
    }

    #[test]
    fn test_parse_server_specs_empty_and_whitespace() {
        let results = parse::parse_server_specs("  ; ;rust:ra;  ");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "rust");
        assert_eq!(results[0].1.command, "ra");
    }

    #[test]
    fn test_resolve_language_servers() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.tsserver]
command = "typescript-language-server"

[language.typescript]
servers = ["tsserver"]
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        let resolved = config
            .resolve_language("typescript")
            .expect("should resolve");
        assert_eq!(resolved.servers, vec![ServerBinding::new("tsserver")]);

        // Unconfigured language returns None
        assert!(config.resolve_language("unknown").is_none());

        Ok(())
    }

    #[test]
    fn test_config_check_valid() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"

[language.rust]
servers = ["rust-analyzer"]
"#,
        )?;

        // check() should succeed for a valid config
        let config = Config::load_from_sources(&[config_path]);
        assert!(config.is_ok());

        Ok(())
    }

    #[test]
    fn test_config_check_invalid_old_format() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[language.rust]
command = "rust-analyzer"
"#,
        )
        .expect("write config");

        // check() should fail for old inline format
        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("[server.*]"),
            "error should mention server migration: {err}",
        );
    }

    #[test]
    fn test_config_check_fast() {
        // Config check must complete in < 50ms — regression guard.
        // We check against an empty config (fastest path).
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "").expect("write config");

        let start = std::time::Instant::now();
        let _ = Config::load_from_sources(&[config_path]);
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "config check took {elapsed:?}, expected < 50ms",
        );
    }

    #[test]
    fn notification_config_default() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "")?;

        let config = Config::load_from_sources(&[config_path])?;
        assert!(config.notifications.is_none());
        assert_eq!(
            config.notifications.unwrap_or_default().threshold,
            SeverityConfig::Warn,
        );

        Ok(())
    }

    #[test]
    fn notification_config_parses_all_levels() -> anyhow::Result<()> {
        let dir = tempdir()?;
        for (toml_val, expected) in [
            ("debug", SeverityConfig::Debug),
            ("info", SeverityConfig::Info),
            ("warn", SeverityConfig::Warn),
            ("error", SeverityConfig::Error),
        ] {
            let path = dir.path().join(format!("{toml_val}.toml"));
            fs::write(
                &path,
                format!("[notifications]\nthreshold = \"{toml_val}\"\n"),
            )?;
            let config = Config::load_from_sources(&[path])?;
            assert_eq!(
                config.notifications.expect("should be Some").threshold,
                expected,
            );
        }

        Ok(())
    }

    #[test]
    fn notification_config_rejects_unknown_key() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(&path, "[notifications]\nfoo = \"bar\"\n").expect("write");

        let result = Config::load_from_sources(&[path]);
        assert!(result.is_err());
    }

    #[test]
    fn notification_config_project_overrides_user() -> anyhow::Result<()> {
        let dir = tempdir()?;

        let user = dir.path().join("user.toml");
        fs::write(&user, "[notifications]\nthreshold = \"warn\"\n")?;

        let project = dir.path().join("project.toml");
        fs::write(&project, "[notifications]\nthreshold = \"info\"\n")?;

        let config = Config::load_from_sources(&[user, project])?;
        assert_eq!(
            config.notifications.expect("should be Some").threshold,
            SeverityConfig::Info,
        );

        Ok(())
    }

    #[test]
    fn notification_config_project_absent_falls_through() -> anyhow::Result<()> {
        let dir = tempdir()?;

        let user = dir.path().join("user.toml");
        fs::write(&user, "[notifications]\nthreshold = \"error\"\n")?;

        let project = dir.path().join("project.toml");
        fs::write(&project, "")?;

        let config = Config::load_from_sources(&[user, project])?;
        // Project omits [notifications] entirely — user's value is preserved.
        assert_eq!(
            config.notifications.unwrap_or_default().threshold,
            SeverityConfig::Error,
        );

        Ok(())
    }

    #[test]
    fn severity_config_converts_to_logging_severity() {
        use crate::logging::Severity;

        assert_eq!(Severity::from(SeverityConfig::Debug), Severity::Debug);
        assert_eq!(Severity::from(SeverityConfig::Info), Severity::Info);
        assert_eq!(Severity::from(SeverityConfig::Warn), Severity::Warn);
        assert_eq!(Severity::from(SeverityConfig::Error), Severity::Error);
    }

    #[test]
    fn test_bare_string_binding() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.foo]
command = "foo-server"

[language.test]
servers = ["foo"]
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let lc = config.language.get("test").expect("test language");
        assert_eq!(lc.servers.len(), 1);
        assert_eq!(lc.servers[0].name, "foo");
        assert!(lc.servers[0].diagnostics);

        Ok(())
    }

    #[test]
    fn test_inline_table_binding() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.foo]
command = "foo-server"

[language.test]
servers = [{ name = "foo", diagnostics = false }]
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let lc = config.language.get("test").expect("test language");
        assert_eq!(lc.servers.len(), 1);
        assert_eq!(lc.servers[0].name, "foo");
        assert!(!lc.servers[0].diagnostics);

        Ok(())
    }

    #[test]
    fn test_mixed_binding() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.alpha]
command = "alpha-server"

[server.beta]
command = "beta-server"

[language.test]
servers = ["alpha", { name = "beta", diagnostics = false }]
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let lc = config.language.get("test").expect("test language");
        assert_eq!(lc.servers.len(), 2);
        assert_eq!(
            lc.servers,
            vec![
                ServerBinding::new("alpha"),
                ServerBinding {
                    name: "beta".to_string(),
                    diagnostics: false,
                },
            ],
        );

        Ok(())
    }

    #[test]
    fn test_unknown_binding_key_rejected() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.foo]
command = "foo-server"

[language.test]
servers = [{ name = "foo", typo = true }]
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("typo"),
            "error should mention the unknown key: {err}",
        );
    }

    #[test]
    fn test_language_diagnostics_default() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.foo]
command = "foo-server"

[language.test]
servers = ["foo"]
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let lc = config.language.get("test").expect("test language");
        assert!(lc.diagnostics);

        Ok(())
    }

    #[test]
    fn test_language_diagnostics_false() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.md-server]
command = "md-server"

[language.markdown]
servers = ["md-server"]
diagnostics = false
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let lc = config.language.get("markdown").expect("markdown language");
        assert!(!lc.diagnostics);

        Ok(())
    }

    #[test]
    fn test_diagnostics_enabled_and_logic() {
        // language true, binding true → true
        let lc = LanguageConfig {
            servers: vec![ServerBinding::new("s")],
            ..LanguageConfig::default()
        };
        assert!(lc.diagnostics_enabled("s"));

        // language false, binding true → false
        let lc = LanguageConfig {
            servers: vec![ServerBinding::new("s")],
            diagnostics: false,
        };
        assert!(!lc.diagnostics_enabled("s"));

        // language true, binding false → false
        let lc = LanguageConfig {
            servers: vec![ServerBinding {
                name: "s".to_string(),
                diagnostics: false,
            }],
            ..LanguageConfig::default()
        };
        assert!(!lc.diagnostics_enabled("s"));

        // language false, binding false → false
        let lc = LanguageConfig {
            servers: vec![ServerBinding {
                name: "s".to_string(),
                diagnostics: false,
            }],
            diagnostics: false,
        };
        assert!(!lc.diagnostics_enabled("s"));
    }

    #[test]
    fn test_diagnostics_enabled_unknown_server() {
        let lc = LanguageConfig {
            servers: vec![ServerBinding::new("known")],
            ..LanguageConfig::default()
        };
        assert!(!lc.diagnostics_enabled("unknown"));
    }

    #[test]
    fn test_env_var_creates_binding() {
        let results = parse::parse_server_specs("rust:rust-analyzer");
        assert_eq!(results.len(), 1);

        let (_, _, lang_config) = &results[0];
        assert_eq!(lang_config.servers.len(), 1);
        assert_eq!(lang_config.servers[0].name, "rust");
        assert!(lang_config.servers[0].diagnostics);
    }

    #[test]
    fn test_min_severity_on_server() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.foo]
command = "foo-server"
min_severity = "warning"

[language.test]
servers = ["foo"]
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let server = config.server.get("foo").expect("foo server def");
        assert_eq!(server.min_severity.as_deref(), Some("warning"));

        Ok(())
    }

    #[test]
    fn test_min_severity_on_language_rejected() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.foo]
command = "foo-server"

[language.rust]
servers = ["foo"]
min_severity = "warning"
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("min_severity") && err.contains("[server.*]"),
            "error should mention moving min_severity to server: {err}",
        );
    }

    #[test]
    fn test_min_severity_absent() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.foo]
command = "foo-server"

[language.test]
servers = ["foo"]
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let server = config.server.get("foo").expect("foo server def");
        assert!(server.min_severity.is_none());

        Ok(())
    }
}

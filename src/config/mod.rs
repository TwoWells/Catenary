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

pub use language::LanguageConfig;
pub use server::ServerDef;

/// Overall configuration for Catenary.
#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    /// Global idle timeout in seconds (default: 300).
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: u64,

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

    /// Icon theme configuration.
    #[serde(default)]
    pub icons: IconConfig,

    /// TUI configuration.
    #[serde(default)]
    pub tui: TuiConfig,

    /// True if any config source used the deprecated `[server.*]` key.
    #[serde(skip)]
    pub deprecated_server_key: bool,
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
pub struct TuiConfig {
    /// Automatically add new sessions to the grid (default: true).
    #[serde(default = "default_true")]
    pub auto_add_sessions: bool,

    /// Preferred width of the Sessions tree as a fraction of the terminal
    /// (default: 0.4).
    #[serde(default = "default_sessions_width")]
    pub sessions_width: f64,

    /// Whether mouse hover changes focus (default: false).
    #[serde(default)]
    pub focus_follows_mouse: bool,

    /// Capture full tool output in `ToolResult` events for TUI detail
    /// expansion (default: false). Increases database size.
    #[serde(default)]
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

const fn default_true() -> bool {
    true
}

const fn default_sessions_width() -> f64 {
    0.25
}

pub(crate) const fn default_idle_timeout() -> u64 {
    300
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
    /// - `inherit` targets are missing, chained, or cyclic.
    /// - A concrete language entry is missing `command`.
    pub fn load() -> Result<Self> {
        parse::load()
    }

    /// Load configuration from an explicit list of file paths.
    ///
    /// Sources are merged in order (later overrides earlier). Environment
    /// variable overrides, default inherits, and validation are applied
    /// after merging.
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

    /// Apply default inherit entries for known language variants.
    fn apply_default_inherits(&mut self) {
        parse::apply_default_inherits(self);
    }

    /// Validate the merged config, returning all errors found.
    ///
    /// Returns an empty vec when the config is valid.
    #[must_use]
    pub fn validate(&self) -> Vec<String> {
        validate::validate(self)
    }

    /// Resolve `inherit` for a language key, returning the canonical key
    /// and the effective config.
    ///
    /// If the language has `inherit`, returns the target key and a merged
    /// config (inherit-only overrides applied on top of the base). If no
    /// `inherit`, returns the key and config as-is.
    #[must_use]
    pub fn resolve_language<'a>(&'a self, key: &'a str) -> Option<(&'a str, LanguageConfig)> {
        language::resolve_language(&self.language, key)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_timeout: default_idle_timeout(),
            log_retention_days: default_log_retention_days(),
            language: HashMap::new(),
            server: HashMap::new(),
            icons: IconConfig::default(),
            tui: TuiConfig::default(),
            deprecated_server_key: false,
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
idle_timeout = 42

[server.rust-analyzer]
command = "rust-analyzer-local"

[language.rust]
servers = ["rust-analyzer"]
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        assert_eq!(config.idle_timeout, 42);
        assert_eq!(
            config
                .language
                .get("rust")
                .expect("rust language config")
                .servers,
            vec!["rust-analyzer"],
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
    fn test_inherit_resolves() -> anyhow::Result<()> {
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

[language.typescriptreact]
inherit = "typescript"
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        let (canonical, resolved) = config
            .resolve_language("typescriptreact")
            .expect("should resolve");
        assert_eq!(canonical, "typescript");
        assert_eq!(resolved.servers, vec!["tsserver"]);

        Ok(())
    }

    #[test]
    fn test_inherit_with_override() -> anyhow::Result<()> {
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
min_severity = "warning"

[language.typescriptreact]
inherit = "typescript"
min_severity = "error"
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        let (_, resolved) = config
            .resolve_language("typescriptreact")
            .expect("should resolve");
        assert_eq!(resolved.min_severity.as_deref(), Some("error"));
        assert_eq!(resolved.servers, vec!["tsserver"]);

        Ok(())
    }

    #[test]
    fn test_inherit_missing_target() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[language.typescriptreact]
inherit = "typescript"
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
    }

    #[test]
    fn test_inherit_chain_rejected() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.server-a]
command = "server-a"

[language.a]
servers = ["server-a"]

[language.b]
inherit = "a"

[language.c]
inherit = "b"
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(err.contains("chains"), "error should mention chains: {err}",);
    }

    #[test]
    fn test_inherit_cycle_rejected() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[language.a]
inherit = "b"

[language.b]
inherit = "a"
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
    }

    #[test]
    fn test_concrete_without_servers_rejected() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[language.rust]
min_severity = "warning"
"#,
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
    fn test_default_inherits_applied() -> anyhow::Result<()> {
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

        // Default inherit should have been applied
        assert!(config.language.contains_key("typescriptreact"));
        let (canonical, _) = config
            .resolve_language("typescriptreact")
            .expect("should resolve");
        assert_eq!(canonical, "typescript");

        Ok(())
    }

    #[test]
    fn test_user_defined_overrides_default_inherit() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.tsserver]
command = "typescript-language-server"
args = ["--stdio"]

[server.custom-tsx]
command = "custom-tsx-server"

[language.typescript]
servers = ["tsserver"]

[language.typescriptreact]
servers = ["custom-tsx"]
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        // User-defined entry should win over default inherit
        let tsx = config
            .language
            .get("typescriptreact")
            .expect("typescriptreact config");
        assert!(tsx.inherit.is_none());
        assert_eq!(tsx.servers, vec!["custom-tsx"]);

        Ok(())
    }

    #[test]
    fn test_empty_config() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "")?;

        let config = Config::load_from_sources(&[config_path])?;
        assert_eq!(config.idle_timeout, 300);
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
idle_timeout = 42

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
idle_timeout = 99
",
        )?;

        let config = Config::load_from_sources(&[local_config_path, explicit_path])?;

        assert_eq!(config.idle_timeout, 99);
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

[server.clangd]
command = "clangd"
args = ["--background-index"]

[language.rust]
servers = ["rust-analyzer"]
min_severity = "warning"

[language.c]
servers = ["clangd"]

[language.cpp]
inherit = "c"
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

        // Language entries
        let rust = config.language.get("rust").expect("rust config");
        assert_eq!(rust.servers, vec!["rust-analyzer"]);
        assert_eq!(rust.min_severity.as_deref(), Some("warning"));

        let c = config.language.get("c").expect("c config");
        assert_eq!(c.servers, vec!["clangd"]);

        // Inherit resolves correctly
        let (canonical, resolved) = config.resolve_language("cpp").expect("should resolve");
        assert_eq!(canonical, "c");
        assert_eq!(resolved.servers, vec!["clangd"]);

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
    fn test_inherit_with_servers_error() {
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
servers = ["tsserver"]
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("inherit") && err.contains("servers"),
            "error should mention inherit + servers conflict: {err}",
        );
    }

    /// Verify that `apply_env_overrides` creates both a `ServerDef` and a
    /// `LanguageConfig` for each `CATENARY_SERVERS` spec. We test the
    /// internal function directly on a default config to avoid `set_var`
    /// (unsafe in Rust 2024).
    #[test]
    fn test_env_var_creates_both() {
        let mut config = Config::default();

        // Simulate what apply_env_overrides does for "rust:rust-analyzer --log-level info"
        let server_name = "rust".to_string();
        config.server.insert(
            server_name.clone(),
            ServerDef {
                command: "rust-analyzer".to_string(),
                args: vec!["--log-level".to_string(), "info".to_string()],
                initialization_options: None,
                settings: None,
            },
        );
        config.language.insert(
            "rust".to_string(),
            LanguageConfig {
                servers: vec![server_name],
                min_severity: None,
                inherit: None,
            },
        );

        // Verify both entries are correct
        let server = config.server.get("rust").expect("rust server def");
        assert_eq!(server.command, "rust-analyzer");
        assert_eq!(server.args, vec!["--log-level", "info"]);

        let lang = config.language.get("rust").expect("rust language config");
        assert_eq!(lang.servers, vec!["rust"]);

        // Config should validate
        let errors = config.validate();
        assert!(errors.is_empty(), "should be valid: {errors:?}");
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

[language.typescriptreact]
inherit = "typescript"
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        // Direct resolution
        let (key, resolved) = config
            .resolve_language("typescript")
            .expect("should resolve");
        assert_eq!(key, "typescript");
        assert_eq!(resolved.servers, vec!["tsserver"]);

        // Inherit resolution
        let (key, resolved) = config
            .resolve_language("typescriptreact")
            .expect("should resolve");
        assert_eq!(key, "typescript");
        assert_eq!(resolved.servers, vec!["tsserver"]);

        Ok(())
    }
}

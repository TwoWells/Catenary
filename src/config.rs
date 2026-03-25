// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

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

/// Per-language configuration for how Catenary handles a language.
///
/// Each entry describes which language server to spawn, how to initialize
/// it, what settings to relay via `workspace/configuration`, and how to
/// filter diagnostics. Entries with `inherit` delegate to another language's
/// config (e.g., `typescriptreact` inherits from `typescript`).
#[derive(Debug, Deserialize, Clone)]
pub struct LanguageConfig {
    /// The command to execute (e.g., "rust-analyzer").
    /// Required for concrete entries, absent for inherit-only entries.
    pub command: Option<String>,

    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,

    /// Initialization options to pass to the LSP server.
    #[serde(default)]
    pub initialization_options: Option<serde_json::Value>,

    /// Minimum diagnostic severity to deliver to agents.
    /// Valid values: `"error"`, `"warning"`, `"information"`, `"hint"`.
    /// When absent, all severities are delivered.
    #[serde(default)]
    pub min_severity: Option<String>,

    /// Server-specific settings returned in `workspace/configuration` responses.
    ///
    /// The TOML nesting mirrors the JSON object the server expects.
    /// Catenary does not interpret these settings — it matches the
    /// `section` path from configuration requests and returns the subtree.
    #[serde(default)]
    pub settings: Option<serde_json::Value>,

    /// Inherit configuration from another language entry.
    /// The target must be a concrete entry (no chains).
    #[serde(default)]
    pub inherit: Option<String>,
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

const fn default_idle_timeout() -> u64 {
    300
}

const fn default_log_retention_days() -> i64 {
    7
}

/// Known language variants that inherit from a base language by default.
///
/// Applied when the user's config has the base language but no explicit
/// entry for the variant. User-defined entries take precedence.
const DEFAULT_INHERIT: &[(&str, &str)] = &[
    ("typescriptreact", "typescript"),
    ("javascriptreact", "javascript"),
];

impl Config {
    /// Load configuration from standard paths or a specific file.
    ///
    /// Sources are loaded in order, with later sources overriding earlier ones:
    /// 1. User config (`~/.config/catenary/config.toml`)
    /// 2. Project-local config (`.catenary.toml`, searching upward from cwd)
    /// 3. Explicit file (if provided)
    /// 4. Environment variable overrides
    ///
    /// The deprecated `[server.*]` key is accepted as an alias for
    /// `[language.*]`. If both are present in the same file, an error is
    /// returned.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A configuration file exists but cannot be read or parsed.
    /// - A file uses both `[server.*]` and `[language.*]`.
    /// - `inherit` targets are missing, chained, or cyclic.
    /// - A concrete language entry is missing `command`.
    pub fn load(explicit_file: Option<PathBuf>) -> Result<Self> {
        let mut sources: Vec<PathBuf> = Vec::new();

        // 1. User config directory (~/.config/catenary/config.toml)
        if let Some(config_dir) = dirs::config_dir() {
            let config_path = config_dir.join("catenary").join("config.toml");
            if config_path.exists() {
                sources.push(config_path);
            }
        }

        // 2. Project-local config (.catenary.toml) searching upwards
        if let Ok(cwd) = std::env::current_dir() {
            let mut current = Some(cwd.as_path());
            while let Some(path) = current {
                let config_path = path.join(".catenary.toml");
                if config_path.exists() {
                    sources.push(config_path);
                    break;
                }
                current = path.parent();
            }
        }

        // 3. Explicit file
        if let Some(path) = explicit_file {
            sources.push(path);
        }

        Self::load_from_sources(&sources)
    }

    /// Load configuration from an explicit list of file paths.
    ///
    /// Sources are merged in order (later overrides earlier). Environment
    /// variable overrides, default inherits, and validation are applied
    /// after merging.
    fn load_from_sources(sources: &[PathBuf]) -> Result<Self> {
        let mut config = Self::default();
        for source in sources {
            let contents = std::fs::read_to_string(source)
                .with_context(|| format!("Failed to read config file: {}", source.display()))?;
            let (layer, deprecated) = Self::deserialize_source(&contents)
                .with_context(|| format!("Failed to parse config file: {}", source.display()))?;
            if deprecated {
                config.deprecated_server_key = true;
            }
            config.merge(layer);
        }

        config.apply_env_overrides();
        config.apply_default_inherits();

        let errors = config.validate();
        if !errors.is_empty() {
            bail!("Configuration errors:\n{}", errors.join("\n"));
        }

        Ok(config)
    }

    /// Deserialize a TOML source, handling the `[server.*]` → `[language.*]`
    /// migration. Returns the deserialized config and whether the deprecated
    /// key was present.
    fn deserialize_source(contents: &str) -> Result<(Self, bool)> {
        // Parse to Value first to detect deprecated [server.*] key
        let raw: toml::Value = toml::from_str(contents).context("Failed to parse TOML")?;

        let has_server = raw.get("server").is_some();
        let has_language = raw.get("language").is_some();

        if has_server && has_language {
            bail!(
                "Config contains both [server.*] and [language.*] — \
                 rename [server.*] to [language.*] and remove [server.*]"
            );
        }

        // If using deprecated key, rewrite and re-serialize so that
        // toml::from_str applies all serde defaults correctly.
        let config: Self = if has_server {
            let mut table = raw.as_table().cloned().unwrap_or_default();
            if let Some(server_val) = table.remove("server") {
                table.insert("language".to_string(), server_val);
            }
            let rewritten = toml::to_string(&toml::Value::Table(table))
                .context("Failed to re-serialize migrated config")?;
            toml::from_str(&rewritten).context("Failed to deserialize configuration")?
        } else {
            toml::from_str(contents).context("Failed to deserialize configuration")?
        };

        Ok((config, has_server))
    }

    /// Merge another config layer into this one. Later values override.
    fn merge(&mut self, other: Self) {
        if other.idle_timeout != default_idle_timeout() {
            self.idle_timeout = other.idle_timeout;
        }
        if other.log_retention_days != default_log_retention_days() {
            self.log_retention_days = other.log_retention_days;
        }
        for (key, value) in other.language {
            self.language.insert(key, value);
        }
        // Icons and TUI: override if the source provided them.
        // Since we can't distinguish "user set default" from "absent",
        // we always take the later source's values for structured sections.
        // This matches the previous config crate behavior.
        self.icons = other.icons;
        self.tui = other.tui;
    }

    /// Apply environment variable overrides for supported keys.
    fn apply_env_overrides(&mut self) {
        if let Ok(val) = std::env::var("CATENARY_IDLE_TIMEOUT")
            && let Ok(v) = val.parse()
        {
            self.idle_timeout = v;
        }
        if let Ok(val) = std::env::var("CATENARY_LOG_RETENTION_DAYS")
            && let Ok(v) = val.parse()
        {
            self.log_retention_days = v;
        }
    }

    /// Apply default inherit entries for known language variants.
    fn apply_default_inherits(&mut self) {
        for &(variant, base) in DEFAULT_INHERIT {
            // Only apply if the base language is configured and the
            // variant is not explicitly defined by the user.
            if self.language.contains_key(base) && !self.language.contains_key(variant) {
                self.language.insert(
                    variant.to_string(),
                    LanguageConfig {
                        command: None,
                        args: Vec::new(),
                        initialization_options: None,
                        min_severity: None,
                        settings: None,
                        inherit: Some(base.to_string()),
                    },
                );
            }
        }
    }

    /// Validate the merged config, returning all errors found.
    ///
    /// Returns an empty vec when the config is valid.
    #[must_use]
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();

        for (key, lang_config) in &self.language {
            if let Some(ref target) = lang_config.inherit {
                match self.language.get(target) {
                    None => {
                        errors.push(format!(
                            "Language '{key}' inherits from '{target}', \
                             but '{target}' is not configured"
                        ));
                    }
                    Some(target_config) if target_config.inherit.is_some() => {
                        errors.push(format!(
                            "Language '{key}' inherits from '{target}', \
                             but '{target}' also inherits — chains are not allowed"
                        ));
                    }
                    _ => {}
                }
            } else if lang_config.command.is_none() {
                errors.push(format!(
                    "Language '{key}' has no `command` and no `inherit` — \
                     concrete entries must specify a command"
                ));
            }
        }

        errors
    }

    /// Resolve `inherit` for a language key, returning the canonical key
    /// and the effective config.
    ///
    /// If the language has `inherit`, returns the target key and a merged
    /// config (inherit-only overrides applied on top of the base). If no
    /// `inherit`, returns the key and config as-is.
    #[must_use]
    pub fn resolve_language<'a>(&'a self, key: &'a str) -> Option<(&'a str, LanguageConfig)> {
        let lang_config = self.language.get(key)?;

        if let Some(ref target) = lang_config.inherit {
            let base = self.language.get(target.as_str())?;
            let mut resolved = base.clone();

            // Apply per-variant overrides from the inheriting entry
            if lang_config.command.is_some() {
                resolved.command.clone_from(&lang_config.command);
            }
            if !lang_config.args.is_empty() {
                resolved.args.clone_from(&lang_config.args);
            }
            if lang_config.initialization_options.is_some() {
                resolved
                    .initialization_options
                    .clone_from(&lang_config.initialization_options);
            }
            if lang_config.min_severity.is_some() {
                resolved.min_severity.clone_from(&lang_config.min_severity);
            }
            if lang_config.settings.is_some() {
                resolved.settings.clone_from(&lang_config.settings);
            }
            resolved.inherit = None;

            Some((target.as_str(), resolved))
        } else {
            Some((key, lang_config.clone()))
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_timeout: default_idle_timeout(),
            log_retention_days: default_log_retention_days(),
            language: HashMap::new(),
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
    fn test_config_load_local() -> Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
idle_timeout = 42

[language.rust]
command = "rust-analyzer-local"
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        assert_eq!(config.idle_timeout, 42);
        assert_eq!(
            config
                .language
                .get("rust")
                .expect("rust language config")
                .command
                .as_deref(),
            Some("rust-analyzer-local"),
        );
        assert!(!config.deprecated_server_key);

        Ok(())
    }

    #[test]
    fn test_deprecated_server_key() -> Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust]
command = "rust-analyzer"
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        assert!(config.deprecated_server_key);
        assert_eq!(
            config
                .language
                .get("rust")
                .expect("rust language config")
                .command
                .as_deref(),
            Some("rust-analyzer"),
        );

        Ok(())
    }

    #[test]
    fn test_both_server_and_language_errors() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust]
command = "rust-analyzer"

[language.python]
command = "pyright"
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("both"),
            "error should mention both keys: {err}",
        );
    }

    #[test]
    fn test_inherit_resolves() -> Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[language.typescript]
command = "typescript-language-server"
args = ["--stdio"]

[language.typescriptreact]
inherit = "typescript"
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        let (canonical, resolved) = config
            .resolve_language("typescriptreact")
            .expect("should resolve");
        assert_eq!(canonical, "typescript");
        assert_eq!(
            resolved.command.as_deref(),
            Some("typescript-language-server")
        );
        assert_eq!(resolved.args, vec!["--stdio"]);

        Ok(())
    }

    #[test]
    fn test_inherit_with_override() -> Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[language.typescript]
command = "typescript-language-server"
args = ["--stdio"]
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
        assert_eq!(
            resolved.command.as_deref(),
            Some("typescript-language-server")
        );

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
[language.a]
command = "server-a"

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
    fn test_concrete_without_command_rejected() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[language.rust]
args = ["--stdio"]
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("command"),
            "error should mention command: {err}",
        );
    }

    #[test]
    fn test_default_inherits_applied() -> Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[language.typescript]
command = "typescript-language-server"
args = ["--stdio"]
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
    fn test_user_defined_overrides_default_inherit() -> Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[language.typescript]
command = "typescript-language-server"
args = ["--stdio"]

[language.typescriptreact]
command = "custom-tsx-server"
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        // User-defined entry should win over default inherit
        let tsx = config
            .language
            .get("typescriptreact")
            .expect("typescriptreact config");
        assert!(tsx.inherit.is_none());
        assert_eq!(tsx.command.as_deref(), Some("custom-tsx-server"));

        Ok(())
    }

    #[test]
    fn test_empty_config() -> Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "")?;

        let config = Config::load_from_sources(&[config_path])?;
        assert_eq!(config.idle_timeout, 300);
        assert_eq!(config.log_retention_days, 7);
        assert!(config.language.is_empty());

        Ok(())
    }

    #[test]
    fn test_merge_later_source_overrides() -> Result<()> {
        let dir = tempdir()?;

        let local_config_path = dir.path().join(".catenary.toml");
        fs::write(
            &local_config_path,
            r#"
idle_timeout = 42

[language.rust]
command = "rust-analyzer-local"
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
}

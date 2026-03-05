// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

use anyhow::{Context, Result};
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

    /// Server definitions keyed by language ID (e.g., "rust", "python").
    #[serde(default)]
    pub server: HashMap<String, ServerConfig>,

    /// Icon theme configuration.
    #[serde(default)]
    pub icons: IconConfig,

    /// TUI configuration.
    #[serde(default)]
    pub tui: TuiConfig,
}

/// Configuration for a specific LSP server.
#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    /// The command to execute (e.g., "rust-analyzer").
    pub command: String,

    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,

    /// Initialization options to pass to the LSP server.
    #[serde(default)]
    pub initialization_options: Option<serde_json::Value>,
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

impl Config {
    /// Load configuration from standard paths or a specific file.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Default values cannot be set.
    /// - The configuration file exists but cannot be read or parsed.
    /// - The configuration cannot be deserialized into the `Config` struct.
    pub fn load(explicit_file: Option<PathBuf>) -> Result<Self> {
        let mut builder = config::Config::builder();

        // 1. Start with defaults
        builder = builder.set_default("idle_timeout", 300)?;
        builder = builder.set_default("log_retention_days", 7)?;
        builder = builder.set_default("icons.preset", "unicode")?;

        // 2. Load from user config directory (~/.config/catenary/config.toml)
        if let Some(config_dir) = dirs::config_dir() {
            let config_path = config_dir.join("catenary").join("config.toml");
            if config_path.exists() {
                builder = builder.add_source(config::File::from(config_path));
            }
        }

        // 3. Load from project-local config (.catenary.toml) searching upwards
        if let Ok(cwd) = std::env::current_dir() {
            let mut current = Some(cwd.as_path());
            while let Some(path) = current {
                let config_path = path.join(".catenary.toml");
                if config_path.exists() {
                    builder = builder.add_source(config::File::from(config_path));
                    break;
                }
                current = path.parent();
            }
        }

        // 4. Load from explicit file if provided
        if let Some(path) = explicit_file {
            builder = builder.add_source(config::File::from(path));
        }

        // 4. Load from environment variables (CATENARY_IDLE_TIMEOUT, etc.)
        // Use "__" as separator for nested keys (e.g. CATENARY_ICONS__PRESET=nerd).
        builder = builder.add_source(config::Environment::with_prefix("CATENARY").separator("__"));

        let config = builder.build().context("Failed to build configuration")?;

        config
            .try_deserialize()
            .context("Failed to deserialize configuration")
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

        let local_config_path = dir.path().join(".catenary.toml");

        fs::write(
            &local_config_path,
            r#"

    idle_timeout = 42



    [server.rust]

    command = "rust-analyzer-local"

    "#,
        )?;

        // Change current directory to the temp dir

        let original_dir = std::env::current_dir()?;

        std::env::set_current_dir(dir.path())?;

        let config = Config::load(None)?;

        // Restore current directory

        std::env::set_current_dir(original_dir)?;

        assert_eq!(config.idle_timeout, 42);

        assert_eq!(
            config
                .server
                .get("rust")
                .expect("rust server config")
                .command,
            "rust-analyzer-local"
        );

        Ok(())
    }
}

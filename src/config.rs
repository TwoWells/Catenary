/*
 * Copyright (C) 2026 Mark Wells Dev
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

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

    /// Wait for LSP servers to be ready before executing requests (default: true).
    #[serde(default = "default_smart_wait")]
    pub smart_wait: bool,

    /// Server definitions keyed by language ID (e.g., "rust", "python").
    #[serde(default)]
    pub server: HashMap<String, ServerConfig>,
}

const fn default_smart_wait() -> bool {
    true
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

const fn default_idle_timeout() -> u64 {
    300
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
        builder = builder.set_default("smart_wait", true)?;

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
        builder = builder.add_source(config::Environment::with_prefix("CATENARY"));

        let config = builder.build().context("Failed to build configuration")?;

        config
            .try_deserialize()
            .context("Failed to deserialize configuration")
    }
}

#[cfg(test)]
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

    smart_wait = false



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

        assert!(!config.smart_wait);

        assert_eq!(
            config
                .server
                .get("rust")
                .context("missing rust server")?
                .command,
            "rust-analyzer-local"
        );

        Ok(())
    }
}

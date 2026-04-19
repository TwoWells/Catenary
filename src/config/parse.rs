// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! TOML deserialization, file reading, source merging, and env var overrides.

use anyhow::{Context, Result, bail};
use std::path::PathBuf;

use super::{Config, LanguageConfig, ServerBinding, ServerDef, default_log_retention_days};

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
pub fn load() -> Result<Config> {
    let sources = config_sources();
    load_from_sources(&sources)
}

/// Discover configuration file paths in standard order.
///
/// Returns the list of paths that would be loaded (later overrides earlier):
/// 1. User config (`~/.config/catenary/config.toml`)
/// 2. Project-local config (`.catenary.toml`, searching upward from cwd)
/// 3. Explicit file from `CATENARY_CONFIG` env var
#[must_use]
pub fn config_sources() -> Vec<PathBuf> {
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

    // 3. Explicit file from CATENARY_CONFIG env var
    if let Ok(path) = std::env::var("CATENARY_CONFIG") {
        sources.push(PathBuf::from(path));
    }

    sources
}

/// Load configuration from an explicit list of file paths.
///
/// Sources are merged in order (later overrides earlier). Environment
/// variable overrides, default inherits, and validation are applied
/// after merging.
pub fn load_from_sources(sources: &[PathBuf]) -> Result<Config> {
    let mut config = Config::default();
    for source in sources {
        let contents = std::fs::read_to_string(source)
            .with_context(|| format!("Failed to read config file: {}", source.display()))?;
        let layer = deserialize_source(&contents)
            .with_context(|| format!("Failed to parse config file: {}", source.display()))?;
        config.merge(layer);
    }

    config.apply_env_overrides();

    let errors = config.validate();
    if !errors.is_empty() {
        bail!("Configuration errors:\n{}", errors.join("\n"));
    }

    Ok(config)
}

/// Keys that belong on `ServerDef`, not `LanguageConfig`.
pub const SERVER_DEF_KEYS: &[&str] = &["command", "args", "initialization_options", "settings"];

/// Deserialize a TOML source, handling the `[server.*]` / `[language.*]`
/// disambiguation.
///
/// Three cases:
/// - `[server.*]` with `command` fields and NO `[language.*]` → old deprecated
///   format. **Hard error** directing the user to `catenary doctor`.
/// - Both `[server.*]` and `[language.*]` → new format. `[server.*]` entries
///   are parsed as `ServerDef`.
/// - Only `[language.*]` (or neither) → intermediate/new format, parsed directly.
///
/// Additionally, `[language.*]` entries containing inline server definition
/// fields (`command`, `args`, `initialization_options`, `settings`) are
/// rejected with a migration message — these fields now live in `[server.*]`.
fn deserialize_source(contents: &str) -> Result<Config> {
    let raw: toml::Value = toml::from_str(contents).context("Failed to parse TOML")?;

    let has_server = raw.get("server").is_some();
    let has_language = raw.get("language").is_some();

    if has_server && !has_language {
        // Old deprecated format: [server.*] used as language-keyed entries.
        // Check if any entry has a `command` field (distinguishes old format
        // from an accidental empty [server.*] table).
        let is_old_format = raw
            .get("server")
            .and_then(toml::Value::as_table)
            .is_some_and(|t| {
                t.values().any(|v| {
                    v.as_table()
                        .is_some_and(|entry| entry.contains_key("command"))
                })
            });

        if is_old_format {
            bail!(
                "Config uses deprecated [server.*] key for language definitions — \
                 rename [server.*] entries to [language.*] and define servers \
                 in [server.*] with the new format. Run `catenary doctor` for guidance."
            );
        }
    }

    // Reject [language.*] entries that contain inline server definition fields.
    // These fields now belong in [server.*].
    if let Some(lang_table) = raw.get("language").and_then(toml::Value::as_table) {
        for (lang_key, entry) in lang_table {
            if let Some(entry_table) = entry.as_table() {
                if entry_table.contains_key("inherit") {
                    bail!(
                        "[language.{lang_key}] uses the removed `inherit` field — \
                         copy the base language's `servers` list into \
                         [language.{lang_key}] instead. Run `catenary doctor` for guidance.",
                    );
                }

                let stale: Vec<&str> = SERVER_DEF_KEYS
                    .iter()
                    .copied()
                    .filter(|k| entry_table.contains_key(*k))
                    .collect();
                if !stale.is_empty() {
                    bail!(
                        "[language.{lang_key}] contains server definition fields ({}) — \
                         these now belong in [server.*]. Move them to a [server.*] \
                         entry and reference it via `servers = [\"...\"]` in \
                         [language.{lang_key}]. Run `catenary doctor` for guidance.",
                        stale.join(", "),
                    );
                }
            }
        }
    }

    // Both present or only language/neither: parse normally.
    // The `server` field on Config maps to [server.*] as ServerDef entries.
    let config: Config = toml::from_str(contents).context("Failed to deserialize configuration")?;

    Ok(config)
}

/// Merge another config layer into this one. Later values override.
///
/// # Merge strategies
///
/// **Scalars** (`log_retention_days`): override only when the later
/// source differs from the default. Cannot distinguish "user explicitly
/// set the default" from "absent", but acceptable for simple numeric knobs.
///
/// **Maps** (`language`, `server`): key-level merge. Later source wins
/// per-key; keys absent from the later source are preserved.
///
/// **Structured sections** (`notifications`, `icons`, `tui`): `Option<T>`
/// on `Config`. `None` means the source did not mention the section;
/// `Some` means it was present (even if all values match defaults). Merge
/// only overwrites when the later source is `Some`, so an earlier source's
/// explicit setting survives an unrelated later source. **All config
/// sections should follow this pattern.**
pub(super) fn merge(config: &mut Config, other: Config) {
    if other.log_retention_days != default_log_retention_days() {
        config.log_retention_days = other.log_retention_days;
    }
    for (key, value) in other.language {
        config.language.insert(key, value);
    }
    for (key, value) in other.server {
        config.server.insert(key, value);
    }
    if other.notifications.is_some() {
        config.notifications = other.notifications;
    }
    if other.icons.is_some() {
        config.icons = other.icons;
    }
    if other.tui.is_some() {
        config.tui = other.tui;
    }
}

/// Apply environment variable overrides for supported keys.
pub(super) fn apply_env_overrides(config: &mut Config) {
    if let Ok(val) = std::env::var("CATENARY_LOG_RETENTION_DAYS")
        && let Ok(v) = val.parse()
    {
        config.log_retention_days = v;
    }

    // CATENARY_SERVERS: semicolon-separated "lang:command args" specs
    if let Ok(val) = std::env::var("CATENARY_SERVERS") {
        for (lang, server_def, lang_config) in parse_server_specs(&val) {
            config.server.insert(lang.clone(), server_def);
            config.language.insert(lang, lang_config);
        }
    }
}

/// Parse a `CATENARY_SERVERS` value into `(lang, ServerDef, LanguageConfig)` triples.
///
/// Format: semicolon-separated `"lang:command args"` specs. The language
/// key doubles as the server name for env-derived entries.
pub(super) fn parse_server_specs(val: &str) -> Vec<(String, ServerDef, LanguageConfig)> {
    let mut results = Vec::new();
    for spec in val.split(';') {
        let spec = spec.trim();
        if spec.is_empty() {
            continue;
        }
        if let Some((lang, command_str)) = spec.split_once(':') {
            let lang = lang.trim();
            let command_str = command_str.trim();
            let mut parts = command_str.split_whitespace();
            if let Some(program) = parts.next() {
                let cmd_args: Vec<String> = parts.map(std::string::ToString::to_string).collect();
                let server_name = lang.to_string();
                results.push((
                    lang.to_string(),
                    ServerDef {
                        command: program.to_string(),
                        args: cmd_args,
                        initialization_options: None,
                        settings: None,
                    },
                    LanguageConfig {
                        servers: vec![ServerBinding::new(server_name)],
                        ..LanguageConfig::default()
                    },
                ));
            }
        }
    }
    results
}

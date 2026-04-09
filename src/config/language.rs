// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Language configuration and inherit resolution.

use serde::Deserialize;

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

/// Known language variants that inherit from a base language by default.
///
/// Applied when the user's config has the base language but no explicit
/// entry for the variant. User-defined entries take precedence.
pub const DEFAULT_INHERIT: &[(&str, &str)] = &[
    ("typescriptreact", "typescript"),
    ("javascriptreact", "javascript"),
];

/// Resolve `inherit` for a language key against a language map, returning
/// the canonical key and the effective config.
///
/// If the language has `inherit`, returns the target key and a merged
/// config (inherit-only overrides applied on top of the base). If no
/// `inherit`, returns the key and config as-is.
#[must_use]
pub fn resolve_language<'a>(
    language: &'a std::collections::HashMap<String, LanguageConfig>,
    key: &'a str,
) -> Option<(&'a str, LanguageConfig)> {
    let lang_config = language.get(key)?;

    if let Some(ref target) = lang_config.inherit {
        let base = language.get(target.as_str())?;
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

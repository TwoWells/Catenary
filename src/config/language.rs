// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Language configuration and inherit resolution.

use serde::Deserialize;

/// Per-language configuration for how Catenary handles a language.
///
/// Each entry references one or more server definitions from `[server.*]`
/// via the `servers` list, controls diagnostic severity filtering, and
/// supports inherit-based delegation (e.g., `typescriptreact` inherits
/// from `typescript`).
#[derive(Debug, Deserialize, Clone)]
pub struct LanguageConfig {
    /// Ordered list of server names (references `[server.*]` entries).
    /// Order defines dispatch priority.
    #[serde(default)]
    pub servers: Vec<String>,

    /// Minimum diagnostic severity to deliver to agents.
    /// Valid values: `"error"`, `"warning"`, `"information"`, `"hint"`.
    /// When absent, all severities are delivered.
    #[serde(default)]
    pub min_severity: Option<String>,

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
///
/// Inherit resolution for `servers`: if the inheriting entry has a
/// non-empty `servers` list, it overrides the base's list. Otherwise
/// the base's `servers` list is used.
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
        if !lang_config.servers.is_empty() {
            resolved.servers.clone_from(&lang_config.servers);
        }
        if lang_config.min_severity.is_some() {
            resolved.min_severity.clone_from(&lang_config.min_severity);
        }
        resolved.inherit = None;

        Some((target.as_str(), resolved))
    } else {
        Some((key, lang_config.clone()))
    }
}

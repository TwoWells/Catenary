// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Language configuration and alias resolution.

use serde::Deserialize;

/// Per-language configuration for how Catenary handles a language.
///
/// Each entry references one or more server definitions from `[server.*]`
/// via the `servers` list and controls diagnostic severity filtering.
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
}

/// Known language variants that map to a base language.
///
/// Consulted at lookup time when the user's config has the base language
/// but no explicit entry for the variant. User-defined entries take
/// precedence over aliases.
pub const LANGUAGE_ALIASES: &[(&str, &str)] = &[
    ("typescriptreact", "typescript"),
    ("javascriptreact", "javascript"),
];

/// Resolve a language key against a language map, returning the canonical
/// key and the effective config.
///
/// Direct lookup is tried first. If the key is not found, the alias table
/// is consulted to redirect to a base language entry.
#[must_use]
pub fn resolve_language<'a>(
    language: &'a std::collections::HashMap<String, LanguageConfig>,
    key: &'a str,
) -> Option<(&'a str, &'a LanguageConfig)> {
    // Direct lookup
    if let Some(config) = language.get(key) {
        return Some((key, config));
    }
    // Alias fallback
    let target = LANGUAGE_ALIASES
        .iter()
        .find(|(variant, _)| *variant == key)
        .map(|(_, base)| *base)?;
    language.get(target).map(|config| (target, config))
}

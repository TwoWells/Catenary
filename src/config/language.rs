// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Language configuration.

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

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Configuration validation.

use super::Config;

/// Validate the merged config, returning all errors found.
///
/// Returns an empty vec when the config is valid.
#[must_use]
pub fn validate(config: &Config) -> Vec<String> {
    let mut errors = Vec::new();

    // Validate language entries
    for (key, lang_config) in &config.language {
        if lang_config.servers.is_empty() {
            errors.push(format!(
                "Language '{key}' has no `servers` — \
                 every language entry must specify a servers list"
            ));
        }

        // Validate server references
        for server_name in &lang_config.servers {
            if !config.server.contains_key(server_name) {
                errors.push(format!(
                    "Language '{key}' references server '{server_name}', \
                     but no [server.{server_name}] is defined"
                ));
            }
        }
    }

    // Validate server definitions
    for (name, server_def) in &config.server {
        if server_def.command.is_empty() {
            errors.push(format!(
                "Server '{name}' has an empty `command` — \
                 server definitions must specify a command"
            ));
        }
    }

    errors
}

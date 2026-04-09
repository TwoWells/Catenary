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
        if let Some(ref target) = lang_config.inherit {
            // Inherit entries must not have their own servers list
            if !lang_config.servers.is_empty() {
                errors.push(format!(
                    "Language '{key}' has both `inherit` and `servers` — \
                     inherit entries must not specify servers"
                ));
            }
            match config.language.get(target) {
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
        } else if lang_config.servers.is_empty() {
            errors.push(format!(
                "Language '{key}' has no `servers` and no `inherit` — \
                 concrete entries must specify a servers list"
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

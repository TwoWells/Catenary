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
        } else if lang_config.command.is_none() {
            errors.push(format!(
                "Language '{key}' has no `command` and no `inherit` — \
                 concrete entries must specify a command"
            ));
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

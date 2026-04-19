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
        // Entries that have servers OR no classification are expected to
        // have a non-empty servers list.  Classification-only entries
        // (from the default config) are valid without servers.
        if lang_config.servers.is_empty() && !lang_config.has_classification() {
            errors.push(format!(
                "Language '{key}' has no `servers` and no classification fields — \
                 every language entry must specify a servers list or classification"
            ));
        }

        // Validate server references
        for binding in &lang_config.servers {
            if !config.server.contains_key(&binding.name) {
                errors.push(format!(
                    "Language '{key}' references server '{}', \
                     but no [server.{}] is defined",
                    binding.name, binding.name,
                ));
            }
        }

        // Validate classification fields — no empty strings
        if let Some(ref exts) = lang_config.extensions {
            for ext in exts {
                if ext.is_empty() {
                    errors.push(format!(
                        "Language '{key}' has an empty string in `extensions`"
                    ));
                }
            }
        }
        if let Some(ref fnames) = lang_config.filenames {
            for fname in fnames {
                if fname.is_empty() {
                    errors.push(format!(
                        "Language '{key}' has an empty string in `filenames`"
                    ));
                }
            }
        }
        if let Some(ref shebangs) = lang_config.shebangs {
            for shebang in shebangs {
                if shebang.is_empty() {
                    errors.push(format!(
                        "Language '{key}' has an empty string in `shebangs`"
                    ));
                }
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

        // Validate file_patterns — each must be a valid glob, no empty strings
        for pattern in &server_def.file_patterns {
            if pattern.is_empty() {
                errors.push(format!(
                    "Server '{name}' has an empty string in `file_patterns`"
                ));
            } else if let Err(e) = globset::Glob::new(pattern) {
                errors.push(format!(
                    "Server '{name}' has an invalid glob in `file_patterns`: \
                     '{pattern}' — {e}"
                ));
            }
        }
    }

    errors
}

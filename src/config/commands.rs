// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Denylist-based command filter configuration.
//!
//! `[commands]` defines which shell commands are denied and what guidance
//! message to show. Two dispositions: `deny` (always blocked) and
//! `deny_when_first` (blocked at pipe position 0 only). Project config
//! can amend (`allow`/`deny`/`deny_when_first`) or replace
//! (`inherit = false`) the user's set.

use std::collections::HashMap;

use serde::Deserialize;

/// Known template variables in guidance messages.
const KNOWN_VARIABLES: &[&str] = &["read", "edit", "catenary_grep", "catenary_glob"];

/// Top-level `[commands]` config section.
///
/// Deserialized from TOML. Project configs use `allow` to un-deny commands
/// and `inherit` to control whether the user's base set is kept or replaced.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct CommandsConfig {
    /// Inherit user config (project config only, default true).
    #[serde(default = "default_true")]
    pub inherit: bool,
    /// Commands to un-deny (project config only).
    pub allow: Option<Vec<String>>,
    /// Always-deny commands. Key = command name, value = guidance message.
    pub deny: Option<HashMap<String, String>>,
    /// Deny-when-first commands. Key = command name, value = guidance message.
    pub deny_when_first: Option<HashMap<String, String>>,
}

/// A resolved command set after merging user and project configs.
///
/// Keys are either bare command names (`"cat"`) or compound
/// `"command subcommand"` keys (`"git ls-files"`).
#[derive(Debug, Clone, Default)]
pub struct ResolvedCommands {
    /// Always-deny commands. Key -> guidance message.
    pub deny: HashMap<String, String>,
    /// Deny-when-first commands. Key -> guidance message.
    pub deny_when_first: HashMap<String, String>,
}

impl ResolvedCommands {
    /// Merge a later config layer into this resolved set.
    ///
    /// If `layer.inherit` is false, the layer's deny maps replace this set
    /// entirely. Otherwise, `allow` removes keys from both deny maps, and
    /// `deny`/`deny_when_first` add or override keys.
    pub fn merge(&mut self, layer: &CommandsConfig) {
        if !layer.inherit {
            self.deny = layer.deny.clone().unwrap_or_default();
            self.deny_when_first = layer.deny_when_first.clone().unwrap_or_default();
            return;
        }

        // allow removes from both deny sets
        if let Some(ref allow) = layer.allow {
            for key in allow {
                self.deny.remove(key);
                self.deny_when_first.remove(key);
            }
        }

        // deny and deny_when_first add/override
        if let Some(ref deny) = layer.deny {
            for (key, msg) in deny {
                self.deny.insert(key.clone(), msg.clone());
            }
        }
        if let Some(ref deny_when_first) = layer.deny_when_first {
            for (key, msg) in deny_when_first {
                self.deny_when_first.insert(key.clone(), msg.clone());
            }
        }
    }
}

impl Default for CommandsConfig {
    fn default() -> Self {
        Self {
            inherit: true,
            allow: None,
            deny: None,
            deny_when_first: None,
        }
    }
}

const fn default_true() -> bool {
    true
}

/// Validate a `CommandsConfig`, returning all errors found.
///
/// Checks for:
/// - Bare key + compound key collision in same section
/// - Same command in both `deny` and `deny_when_first`
/// - Same command in `allow` and `deny`/`deny_when_first`
/// - `inherit = false` with `allow` present
/// - Unknown template variables (warning, not error)
pub fn validate(config: &CommandsConfig) -> (Vec<String>, Vec<String>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    // inherit = false with allow is contradictory
    if !config.inherit && config.allow.is_some() {
        errors.push(
            "[commands] `inherit = false` with `allow` is contradictory — \
             `allow` removes commands from the inherited set, \
             but `inherit = false` discards it"
                .to_string(),
        );
    }

    // Check for bare + compound key collision within each section
    if let Some(ref deny) = config.deny {
        check_bare_compound_collision(deny, "commands.deny", &mut errors);
    }
    if let Some(ref dwf) = config.deny_when_first {
        check_bare_compound_collision(dwf, "commands.deny_when_first", &mut errors);
    }

    // Same command in both deny and deny_when_first
    if let Some(ref deny) = config.deny
        && let Some(ref dwf) = config.deny_when_first
    {
        for key in deny.keys() {
            if dwf.contains_key(key) {
                errors.push(format!(
                    "[commands] '{key}' appears in both `deny` and \
                     `deny_when_first` — a command can only have one disposition",
                ));
            }
        }
    }

    // Same command in allow and deny/deny_when_first
    if let Some(ref allow) = config.allow {
        if let Some(ref deny) = config.deny {
            for key in allow {
                if deny.contains_key(key) {
                    errors.push(format!(
                        "[commands] '{key}' appears in both `allow` and \
                         `deny` — contradictory entries",
                    ));
                }
            }
        }
        if let Some(ref dwf) = config.deny_when_first {
            for key in allow {
                if dwf.contains_key(key) {
                    errors.push(format!(
                        "[commands] '{key}' appears in both `allow` and \
                         `deny_when_first` — contradictory entries",
                    ));
                }
            }
        }
    }

    // Check template variables in all guidance messages
    let all_messages = config
        .deny
        .iter()
        .flatten()
        .chain(config.deny_when_first.iter().flatten());
    for (key, msg) in all_messages {
        check_template_variables(key, msg, &mut warnings);
    }

    (errors, warnings)
}

/// Check for bare key + compound key collision within a single section.
///
/// E.g., `cargo` and `"cargo test"` in the same deny section is an error.
fn check_bare_compound_collision(
    map: &HashMap<String, String>,
    section: &str,
    errors: &mut Vec<String>,
) {
    // Collect bare keys (no space) and compound keys (has space).
    let bare_keys: Vec<&str> = map
        .keys()
        .filter(|k| !k.contains(' '))
        .map(String::as_str)
        .collect();
    let compound_keys: Vec<&str> = map
        .keys()
        .filter(|k| k.contains(' '))
        .map(String::as_str)
        .collect();

    for compound in &compound_keys {
        // The base command is the part before the first space.
        if let Some(base) = compound.split_once(' ').map(|(b, _)| b)
            && bare_keys.contains(&base)
        {
            errors.push(format!(
                "[{section}] has both '{base}' and '{compound}' — \
                 either deny the command entirely or deny specific \
                 subcommands, not both",
            ));
        }
    }
}

/// Check guidance message for unknown template variables.
fn check_template_variables(key: &str, msg: &str, warnings: &mut Vec<String>) {
    let mut rest = msg;
    while let Some(start) = rest.find('{') {
        let after_brace = &rest[start + 1..];
        if let Some(end) = after_brace.find('}') {
            let var_name = &after_brace[..end];
            if !KNOWN_VARIABLES.contains(&var_name) {
                warnings.push(format!(
                    "[commands] guidance for '{key}' references unknown \
                     template variable '{{{var_name}}}'",
                ));
            }
            rest = &after_brace[end + 1..];
        } else {
            break;
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn default_commands_config() {
        let config = CommandsConfig::default();
        // serde default for bool is false, but we document inherit defaults to true.
        // The struct-level #[serde(default)] uses bool::default() which is false.
        // This is fine — Config::default() won't have a commands section at all
        // (it's Option<CommandsConfig>). The inherit=true default only matters
        // when deserializing from TOML where inherit is absent.
        assert!(config.deny.is_none());
        assert!(config.deny_when_first.is_none());
        assert!(config.allow.is_none());
    }

    #[test]
    fn inherit_defaults_true_from_toml() {
        let config: CommandsConfig = toml::from_str("").expect("empty TOML");
        assert!(config.inherit, "inherit should default to true from TOML");
    }

    #[test]
    fn resolve_user_config_only() {
        let user = CommandsConfig {
            inherit: true,
            allow: None,
            deny: Some(HashMap::from([(
                "cat".to_string(),
                "Use {read} instead".to_string(),
            )])),
            deny_when_first: Some(HashMap::from([(
                "grep".to_string(),
                "Use {catenary_grep} instead".to_string(),
            )])),
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&user);

        assert_eq!(resolved.deny.len(), 1);
        assert_eq!(resolved.deny.get("cat").expect("cat"), "Use {read} instead");
        assert_eq!(resolved.deny_when_first.len(), 1);
        assert_eq!(
            resolved.deny_when_first.get("grep").expect("grep"),
            "Use {catenary_grep} instead",
        );
    }

    #[test]
    fn project_allow_removes_from_user() {
        let user = CommandsConfig {
            inherit: true,
            allow: None,
            deny: Some(HashMap::from([
                ("cat".to_string(), "Use {read}".to_string()),
                ("tail".to_string(), "Use {read}".to_string()),
                ("cargo".to_string(), "Use make".to_string()),
            ])),
            deny_when_first: Some(HashMap::from([(
                "grep".to_string(),
                "Use {catenary_grep}".to_string(),
            )])),
        };

        let project = CommandsConfig {
            inherit: true,
            allow: Some(vec!["tail".to_string(), "cargo".to_string()]),
            deny: None,
            deny_when_first: None,
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&user);
        resolved.merge(&project);

        assert_eq!(resolved.deny.len(), 1);
        assert!(resolved.deny.contains_key("cat"));
        assert!(!resolved.deny.contains_key("tail"));
        assert!(!resolved.deny.contains_key("cargo"));
        assert_eq!(resolved.deny_when_first.len(), 1);
    }

    #[test]
    fn project_deny_adds_to_user() {
        let user = CommandsConfig {
            inherit: true,
            allow: None,
            deny: Some(HashMap::from([(
                "cat".to_string(),
                "Use {read}".to_string(),
            )])),
            deny_when_first: None,
        };

        let project = CommandsConfig {
            inherit: true,
            allow: None,
            deny: Some(HashMap::from([("pip".to_string(), "Use make".to_string())])),
            deny_when_first: Some(HashMap::from([(
                "sed".to_string(),
                "Use {edit}".to_string(),
            )])),
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&user);
        resolved.merge(&project);

        assert_eq!(resolved.deny.len(), 2);
        assert!(resolved.deny.contains_key("cat"));
        assert!(resolved.deny.contains_key("pip"));
        assert_eq!(resolved.deny_when_first.len(), 1);
        assert!(resolved.deny_when_first.contains_key("sed"));
    }

    #[test]
    fn inherit_false_replaces_entirely() {
        let user = CommandsConfig {
            inherit: true,
            allow: None,
            deny: Some(HashMap::from([
                ("cat".to_string(), "Use {read}".to_string()),
                ("tail".to_string(), "Use {read}".to_string()),
            ])),
            deny_when_first: Some(HashMap::from([(
                "grep".to_string(),
                "Use {catenary_grep}".to_string(),
            )])),
        };

        let project = CommandsConfig {
            inherit: false,
            allow: None,
            deny: Some(HashMap::from([("pip".to_string(), "Use make".to_string())])),
            deny_when_first: None,
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&user);
        resolved.merge(&project);

        assert_eq!(resolved.deny.len(), 1);
        assert!(resolved.deny.contains_key("pip"));
        assert!(resolved.deny_when_first.is_empty());
    }

    #[test]
    fn inherit_false_no_deny_clears_all() {
        let user = CommandsConfig {
            inherit: true,
            allow: None,
            deny: Some(HashMap::from([(
                "cat".to_string(),
                "Use {read}".to_string(),
            )])),
            deny_when_first: Some(HashMap::from([(
                "grep".to_string(),
                "Use {catenary_grep}".to_string(),
            )])),
        };

        let project = CommandsConfig {
            inherit: false,
            allow: None,
            deny: None,
            deny_when_first: None,
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&user);
        resolved.merge(&project);

        assert!(resolved.deny.is_empty());
        assert!(resolved.deny_when_first.is_empty());
    }

    #[test]
    fn compound_keys_survive_layering() {
        let user = CommandsConfig {
            inherit: true,
            allow: None,
            deny: Some(HashMap::from([(
                "git ls-files".to_string(),
                "Use {catenary_glob}".to_string(),
            )])),
            deny_when_first: Some(HashMap::from([(
                "git grep".to_string(),
                "Use {catenary_grep}".to_string(),
            )])),
        };

        let project = CommandsConfig {
            inherit: true,
            allow: None,
            deny: Some(HashMap::from([("pip".to_string(), "Use make".to_string())])),
            deny_when_first: None,
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&user);
        resolved.merge(&project);

        assert!(resolved.deny.contains_key("git ls-files"));
        assert!(resolved.deny.contains_key("pip"));
        assert!(resolved.deny_when_first.contains_key("git grep"));
    }

    #[test]
    fn validate_inherit_false_with_allow_error() {
        let config = CommandsConfig {
            inherit: false,
            allow: Some(vec!["cat".to_string()]),
            deny: None,
            deny_when_first: None,
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("inherit = false"));
        assert!(errors[0].contains("allow"));
    }

    #[test]
    fn validate_bare_compound_collision_error() {
        let config = CommandsConfig {
            inherit: true,
            allow: None,
            deny: Some(HashMap::from([
                ("cargo".to_string(), "Use make".to_string()),
                ("cargo test".to_string(), "Use make test".to_string()),
            ])),
            deny_when_first: None,
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("cargo"));
        assert!(errors[0].contains("cargo test"));
    }

    #[test]
    fn validate_cross_section_duplicate_error() {
        let config = CommandsConfig {
            inherit: true,
            allow: None,
            deny: Some(HashMap::from([(
                "grep".to_string(),
                "Use {catenary_grep}".to_string(),
            )])),
            deny_when_first: Some(HashMap::from([(
                "grep".to_string(),
                "Use {catenary_grep}".to_string(),
            )])),
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("grep"));
        assert!(errors[0].contains("deny"));
        assert!(errors[0].contains("deny_when_first"));
    }

    #[test]
    fn validate_allow_deny_contradiction_error() {
        let config = CommandsConfig {
            inherit: true,
            allow: Some(vec!["cat".to_string()]),
            deny: Some(HashMap::from([(
                "cat".to_string(),
                "Use {read}".to_string(),
            )])),
            deny_when_first: None,
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("cat"));
        assert!(errors[0].contains("allow"));
        assert!(errors[0].contains("deny"));
    }

    #[test]
    fn validate_allow_deny_when_first_contradiction_error() {
        let config = CommandsConfig {
            inherit: true,
            allow: Some(vec!["grep".to_string()]),
            deny: None,
            deny_when_first: Some(HashMap::from([(
                "grep".to_string(),
                "Use {catenary_grep}".to_string(),
            )])),
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("grep"));
        assert!(errors[0].contains("allow"));
        assert!(errors[0].contains("deny_when_first"));
    }

    #[test]
    fn validate_unknown_template_variable_warns() {
        let config = CommandsConfig {
            inherit: true,
            allow: None,
            deny: Some(HashMap::from([(
                "cat".to_string(),
                "Use {unknown_tool} instead".to_string(),
            )])),
            deny_when_first: None,
        };

        let (errors, warnings) = validate(&config);
        assert!(errors.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("unknown_tool"));
    }

    #[test]
    fn validate_known_template_variable_no_warning() {
        let config = CommandsConfig {
            inherit: true,
            allow: None,
            deny: Some(HashMap::from([
                ("cat".to_string(), "Use {read} instead".to_string()),
                ("rg".to_string(), "Use {catenary_grep} instead".to_string()),
            ])),
            deny_when_first: None,
        };

        let (errors, warnings) = validate(&config);
        assert!(errors.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn validate_valid_config_no_errors() {
        let config = CommandsConfig {
            inherit: true,
            allow: None,
            deny: Some(HashMap::from([
                ("cat".to_string(), "Use {read}".to_string()),
                (
                    "git ls-files".to_string(),
                    "Use {catenary_glob}".to_string(),
                ),
            ])),
            deny_when_first: Some(HashMap::from([(
                "grep".to_string(),
                "Use {catenary_grep}".to_string(),
            )])),
        };

        let (errors, warnings) = validate(&config);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn project_allow_removes_compound_key() {
        let user = CommandsConfig {
            inherit: true,
            allow: None,
            deny: Some(HashMap::from([
                (
                    "git ls-files".to_string(),
                    "Use {catenary_glob}".to_string(),
                ),
                ("git ls-tree".to_string(), "Use {catenary_glob}".to_string()),
            ])),
            deny_when_first: None,
        };

        let project = CommandsConfig {
            inherit: true,
            allow: Some(vec!["git ls-files".to_string()]),
            deny: None,
            deny_when_first: None,
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&user);
        resolved.merge(&project);

        assert!(!resolved.deny.contains_key("git ls-files"));
        assert!(resolved.deny.contains_key("git ls-tree"));
    }

    #[test]
    fn project_deny_overrides_user_message() {
        let user = CommandsConfig {
            inherit: true,
            allow: None,
            deny: Some(HashMap::from([(
                "cat".to_string(),
                "Old message".to_string(),
            )])),
            deny_when_first: None,
        };

        let project = CommandsConfig {
            inherit: true,
            allow: None,
            deny: Some(HashMap::from([(
                "cat".to_string(),
                "New message".to_string(),
            )])),
            deny_when_first: None,
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&user);
        resolved.merge(&project);

        assert_eq!(resolved.deny.get("cat").expect("cat"), "New message");
    }

    #[test]
    fn three_layer_merge() {
        let user = CommandsConfig {
            inherit: true,
            allow: None,
            deny: Some(HashMap::from([
                ("cat".to_string(), "Use {read}".to_string()),
                ("tail".to_string(), "Use {read}".to_string()),
                ("cargo".to_string(), "Use make".to_string()),
            ])),
            deny_when_first: Some(HashMap::from([(
                "grep".to_string(),
                "Use {catenary_grep}".to_string(),
            )])),
        };

        let project = CommandsConfig {
            inherit: true,
            allow: Some(vec!["cargo".to_string()]),
            deny: Some(HashMap::from([("pip".to_string(), "Use make".to_string())])),
            deny_when_first: None,
        };

        let explicit = CommandsConfig {
            inherit: true,
            allow: Some(vec!["tail".to_string()]),
            deny: None,
            deny_when_first: None,
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&user);
        resolved.merge(&project);
        resolved.merge(&explicit);

        // cat: from user, never removed
        assert!(resolved.deny.contains_key("cat"));
        // tail: from user, removed by explicit
        assert!(!resolved.deny.contains_key("tail"));
        // cargo: from user, removed by project
        assert!(!resolved.deny.contains_key("cargo"));
        // pip: added by project
        assert!(resolved.deny.contains_key("pip"));
        // grep: from user, never removed
        assert!(resolved.deny_when_first.contains_key("grep"));
    }
}

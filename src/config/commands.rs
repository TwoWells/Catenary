// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Allowlist-based command filter configuration.
//!
//! `[commands]` defines which shell commands an agent may run. Three states:
//!
//! 1. **Absent** — no `[commands]` section. Not configured yet; emit a hint
//!    notification once per session at startup.
//! 2. **`client_enforcement_only = true`** — deliberate opt-out. No hint,
//!    no enforcement.
//! 3. **`allow = [...]` present** — active allowlist. Everything not
//!    explicitly allowed is denied.
//!
//! Keys:
//! - `client_enforcement_only` — deliberate opt-out flag.
//! - `build` — the project's build tool (e.g., `"make"`).
//! - `allow` — commands the agent can run unconditionally.
//! - `pipeline` — commands allowed mid-pipeline only (denied at position 0).
//! - `deny.<cmd>` — subcommand denylist within an allowed command.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Top-level `[commands]` config section.
///
/// Deserialized from TOML. The `deny` field uses a nested table:
/// `[commands.deny]` with keys mapping to arrays of denied subcommands
/// (e.g., `git = ["grep", "ls-files"]`).
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct CommandsConfig {
    /// Deliberate opt-out — no enforcement, no hint notification.
    #[serde(default)]
    pub client_enforcement_only: bool,
    /// The project's build tool (e.g., `"make"`).
    pub build: Option<String>,
    /// Commands the agent can run unconditionally.
    pub allow: Option<Vec<String>>,
    /// Commands allowed mid-pipeline only (denied at pipeline position 0).
    pub pipeline: Option<Vec<String>>,
    /// Subcommand denylist within allowed commands.
    /// Key = command name, value = list of denied subcommands.
    pub deny: Option<HashMap<String, Vec<String>>>,
}

/// A resolved command set after merging user and project configs.
#[derive(Debug, Clone, Default)]
pub struct ResolvedCommands {
    /// Deliberate opt-out — no enforcement, no hint notification.
    pub client_enforcement_only: bool,
    /// User-level default build tool (from user config, no root context).
    ///
    /// Used as fallback when `cwd` doesn't match any root in `build`.
    pub default_build: Option<String>,
    /// Per-root build tools. Key = workspace root path, value = build tool name.
    ///
    /// Populated by [`merge_project_commands`](Self::merge_project_commands).
    /// The evaluator looks up `cwd` → root (longest prefix match) → build tool.
    pub build: HashMap<PathBuf, String>,
    /// Commands the agent can run unconditionally.
    pub allow: HashSet<String>,
    /// Commands allowed mid-pipeline only.
    pub pipeline: HashSet<String>,
    /// Subcommand denylist within allowed commands.
    /// Key = command name, value = set of denied subcommands.
    pub deny: HashMap<String, HashSet<String>>,
}

impl ResolvedCommands {
    /// Merge a config layer into this resolved set.
    ///
    /// Each field overwrites when present in the layer. `allow` and `pipeline`
    /// are replaced (not unioned) — the design doc specifies that project
    /// `allow` replaces the user list. `deny` entries are merged per-command.
    /// `build` is stored as `default_build` (user-level, no root context).
    pub fn merge(&mut self, layer: &CommandsConfig) {
        if layer.client_enforcement_only {
            self.client_enforcement_only = true;
        }
        if layer.build.is_some() {
            self.default_build.clone_from(&layer.build);
        }
        if let Some(ref allow) = layer.allow {
            self.allow = allow.iter().cloned().collect();
        }
        if let Some(ref pipeline) = layer.pipeline {
            self.pipeline = pipeline.iter().cloned().collect();
        }
        if let Some(ref deny) = layer.deny {
            for (cmd, subs) in deny {
                self.deny
                    .entry(cmd.clone())
                    .or_default()
                    .extend(subs.iter().cloned());
            }
        }
    }

    /// Merge per-root project commands into this user-level baseline.
    ///
    /// For each root, the project's `allow`/`pipeline`/`deny` replaces the
    /// user baseline for that root's contribution (if set), then all roots'
    /// contributions are unioned. `build` is collected per-root (project
    /// override or user default). Disabled roots contribute commands just
    /// like enabled roots.
    ///
    /// Returns a new `ResolvedCommands` with the merged result.
    #[must_use]
    pub fn merge_project_commands(
        &self,
        roots: &[PathBuf],
        project_commands: &HashMap<PathBuf, CommandsConfig>,
    ) -> Self {
        if roots.is_empty() {
            return self.clone();
        }

        let mut merged_allow: HashSet<String> = HashSet::new();
        let mut merged_pipeline: HashSet<String> = HashSet::new();
        let mut merged_deny: HashMap<String, HashSet<String>> = HashMap::new();
        let mut merged_build: HashMap<PathBuf, String> = HashMap::new();

        for root in roots {
            let project = project_commands.get(root);

            // allow: project replaces user for this root's contribution
            if let Some(project_allow) = project.and_then(|p| p.allow.as_ref()) {
                merged_allow.extend(project_allow.iter().cloned());
            } else {
                merged_allow.extend(self.allow.iter().cloned());
            }

            // pipeline: same replacement semantics
            if let Some(project_pipeline) = project.and_then(|p| p.pipeline.as_ref()) {
                merged_pipeline.extend(project_pipeline.iter().cloned());
            } else {
                merged_pipeline.extend(self.pipeline.iter().cloned());
            }

            // deny: same replacement semantics, then per-command union
            if let Some(project_deny) = project.and_then(|p| p.deny.as_ref()) {
                for (cmd, subs) in project_deny {
                    merged_deny
                        .entry(cmd.clone())
                        .or_default()
                        .extend(subs.iter().cloned());
                }
            } else {
                for (cmd, subs) in &self.deny {
                    merged_deny
                        .entry(cmd.clone())
                        .or_default()
                        .extend(subs.iter().cloned());
                }
            }

            // build: project overrides user default for this root
            let root_build = project
                .and_then(|p| p.build.as_ref())
                .or(self.default_build.as_ref());
            if let Some(build_tool) = root_build {
                merged_build.insert(root.clone(), build_tool.clone());
            }
        }

        Self {
            client_enforcement_only: self.client_enforcement_only,
            default_build: self.default_build.clone(),
            build: merged_build,
            allow: merged_allow,
            pipeline: merged_pipeline,
            deny: merged_deny,
        }
    }

    /// Look up the build tool for a given `cwd`.
    ///
    /// Finds the root whose path is the longest prefix of `cwd` and returns
    /// that root's build tool. Falls back to [`default_build`](Self::default_build)
    /// when no root matches (e.g., no session or `cwd` outside all roots).
    #[must_use]
    pub fn build_for_cwd(&self, cwd: Option<&Path>) -> Option<&str> {
        if let Some(cwd) = cwd
            && let Some(tool) = self
                .build
                .iter()
                .filter(|(root, _)| cwd.starts_with(root))
                .max_by_key(|(root, _)| root.as_os_str().len())
                .map(|(_, tool)| tool.as_str())
        {
            return Some(tool);
        }
        self.default_build.as_deref()
    }

    /// Whether the allowlist is active (has at least one allowed command,
    /// pipeline command, or build tool).
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.client_enforcement_only
            && (!self.allow.is_empty()
                || !self.pipeline.is_empty()
                || !self.build.is_empty()
                || self.default_build.is_some())
    }
}

/// Validate a `CommandsConfig`, returning all errors found.
///
/// Checks for:
/// - `client_enforcement_only` with active config fields
/// - Overlap between `allow` and `pipeline`
/// - `deny` keys not present in `allow`
/// - Empty `allow` or `pipeline` entries
/// - Empty `deny` subcommand entries
pub fn validate(config: &CommandsConfig) -> (Vec<String>, Vec<String>) {
    let mut errors = Vec::new();
    let warnings = Vec::new();

    // client_enforcement_only with active fields is contradictory
    if config.client_enforcement_only
        && (config.allow.is_some()
            || config.pipeline.is_some()
            || config.deny.is_some()
            || config.build.is_some())
    {
        errors.push(
            "[commands] `client_enforcement_only = true` with `allow`, `pipeline`, \
             `deny`, or `build` is contradictory — opt-out means no enforcement"
                .to_string(),
        );
    }

    // Collect allow set for cross-checks
    let allow_set: HashSet<&str> = config
        .allow
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(String::as_str)
        .collect();

    // Check for overlap between allow and pipeline
    if let Some(ref pipeline) = config.pipeline {
        for cmd in pipeline {
            if allow_set.contains(cmd.as_str()) {
                errors.push(format!(
                    "[commands] '{cmd}' appears in both `allow` and `pipeline` — \
                     a command can only be in one list",
                ));
            }
        }
    }

    // deny keys must be in allow (can't deny subcommands of a non-allowed command)
    if let Some(ref deny) = config.deny {
        for cmd in deny.keys() {
            if !allow_set.contains(cmd.as_str()) {
                errors.push(format!(
                    "[commands] deny.{cmd} references '{cmd}' which is not in `allow` — \
                     can only deny subcommands of allowed commands",
                ));
            }
        }
    }

    // Empty strings in allow
    if let Some(ref allow) = config.allow {
        for cmd in allow {
            if cmd.is_empty() {
                errors.push("[commands] `allow` contains an empty string".to_string());
            }
        }
    }

    // Empty strings in pipeline
    if let Some(ref pipeline) = config.pipeline {
        for cmd in pipeline {
            if cmd.is_empty() {
                errors.push("[commands] `pipeline` contains an empty string".to_string());
            }
        }
    }

    // Empty deny subcommand entries
    if let Some(ref deny) = config.deny {
        for (cmd, subs) in deny {
            if subs.is_empty() {
                errors.push(format!(
                    "[commands] deny.{cmd} has an empty subcommand list",
                ));
            }
            for sub in subs {
                if sub.is_empty() {
                    errors.push(format!(
                        "[commands] deny.{cmd} contains an empty subcommand string",
                    ));
                }
            }
        }
    }

    // Empty build string
    if let Some(ref build) = config.build
        && build.is_empty()
    {
        errors.push("[commands] `build` is an empty string".to_string());
    }

    (errors, warnings)
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
        assert!(!config.client_enforcement_only);
        assert!(config.build.is_none());
        assert!(config.allow.is_none());
        assert!(config.pipeline.is_none());
        assert!(config.deny.is_none());
    }

    #[test]
    fn deserialize_empty_toml() {
        let config: CommandsConfig = toml::from_str("").expect("empty TOML");
        assert!(!config.client_enforcement_only);
        assert!(config.build.is_none());
        assert!(config.allow.is_none());
        assert!(config.pipeline.is_none());
        assert!(config.deny.is_none());
    }

    #[test]
    fn deserialize_full_config() {
        let config: CommandsConfig = toml::from_str(
            r#"
build = "make"
allow = ["git", "gh", "cp"]
pipeline = ["grep", "head", "tail"]

[deny]
git = ["grep", "ls-files"]
"#,
        )
        .expect("valid TOML");

        assert_eq!(config.build.as_deref(), Some("make"));
        assert_eq!(config.allow.as_ref().expect("allow").len(), 3);
        assert_eq!(config.pipeline.as_ref().expect("pipeline").len(), 3);
        let deny = config.deny.as_ref().expect("deny");
        assert_eq!(deny.get("git").expect("git deny").len(), 2);
    }

    #[test]
    fn deserialize_client_enforcement_only() {
        let config: CommandsConfig =
            toml::from_str("client_enforcement_only = true").expect("valid TOML");
        assert!(config.client_enforcement_only);
    }

    #[test]
    fn resolve_single_layer() {
        let layer = CommandsConfig {
            build: Some("make".to_string()),
            allow: Some(vec!["git".to_string(), "gh".to_string()]),
            pipeline: Some(vec!["grep".to_string()]),
            deny: Some(HashMap::from([(
                "git".to_string(),
                vec!["grep".to_string()],
            )])),
            ..CommandsConfig::default()
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&layer);

        assert_eq!(resolved.default_build.as_deref(), Some("make"));
        assert!(resolved.allow.contains("git"));
        assert!(resolved.allow.contains("gh"));
        assert!(resolved.pipeline.contains("grep"));
        assert!(resolved.deny.get("git").expect("git").contains("grep"));
    }

    #[test]
    fn project_allow_replaces_user_allow() {
        let user = CommandsConfig {
            allow: Some(vec!["git".to_string(), "gh".to_string(), "cp".to_string()]),
            ..CommandsConfig::default()
        };
        let project = CommandsConfig {
            allow: Some(vec![
                "git".to_string(),
                "gh".to_string(),
                "kubectl".to_string(),
            ]),
            ..CommandsConfig::default()
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&user);
        resolved.merge(&project);

        // Project replaces user's allow list (sequential merge)
        assert!(resolved.allow.contains("git"));
        assert!(resolved.allow.contains("gh"));
        assert!(resolved.allow.contains("kubectl"));
        assert!(!resolved.allow.contains("cp"));
    }

    #[test]
    fn project_build_overrides_user() {
        let user = CommandsConfig {
            build: Some("make".to_string()),
            allow: Some(vec!["git".to_string()]),
            ..CommandsConfig::default()
        };
        let project = CommandsConfig {
            build: Some("npm".to_string()),
            ..CommandsConfig::default()
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&user);
        resolved.merge(&project);

        assert_eq!(resolved.default_build.as_deref(), Some("npm"));
        // User's allow preserved (project didn't specify allow)
        assert!(resolved.allow.contains("git"));
    }

    #[test]
    fn deny_entries_merge_across_layers() {
        let user = CommandsConfig {
            allow: Some(vec!["git".to_string()]),
            deny: Some(HashMap::from([(
                "git".to_string(),
                vec!["grep".to_string()],
            )])),
            ..CommandsConfig::default()
        };
        let project = CommandsConfig {
            deny: Some(HashMap::from([(
                "git".to_string(),
                vec!["ls-files".to_string()],
            )])),
            ..CommandsConfig::default()
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&user);
        resolved.merge(&project);

        let git_deny = resolved.deny.get("git").expect("git deny");
        assert!(git_deny.contains("grep"));
        assert!(git_deny.contains("ls-files"));
    }

    #[test]
    fn client_enforcement_only_sticky() {
        let user = CommandsConfig {
            client_enforcement_only: true,
            ..CommandsConfig::default()
        };
        let project = CommandsConfig {
            allow: Some(vec!["git".to_string()]),
            ..CommandsConfig::default()
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&user);
        resolved.merge(&project);

        assert!(resolved.client_enforcement_only);
    }

    #[test]
    fn is_active_with_allow() {
        let resolved = ResolvedCommands {
            allow: HashSet::from(["git".to_string()]),
            ..ResolvedCommands::default()
        };
        assert!(resolved.is_active());
    }

    #[test]
    fn is_active_with_pipeline() {
        let resolved = ResolvedCommands {
            pipeline: HashSet::from(["grep".to_string()]),
            ..ResolvedCommands::default()
        };
        assert!(resolved.is_active());
    }

    #[test]
    fn is_active_with_default_build() {
        let resolved = ResolvedCommands {
            default_build: Some("make".to_string()),
            ..ResolvedCommands::default()
        };
        assert!(resolved.is_active());
    }

    #[test]
    fn is_active_with_per_root_build() {
        let resolved = ResolvedCommands {
            build: HashMap::from([(PathBuf::from("/project"), "make".to_string())]),
            ..ResolvedCommands::default()
        };
        assert!(resolved.is_active());
    }

    #[test]
    fn is_active_empty() {
        let resolved = ResolvedCommands::default();
        assert!(!resolved.is_active());
    }

    #[test]
    fn is_active_client_enforcement_only() {
        let resolved = ResolvedCommands {
            client_enforcement_only: true,
            allow: HashSet::from(["git".to_string()]),
            ..ResolvedCommands::default()
        };
        assert!(!resolved.is_active());
    }

    // ── Validation tests ────────────────────────────────────────────

    #[test]
    fn validate_valid_config() {
        let config = CommandsConfig {
            build: Some("make".to_string()),
            allow: Some(vec!["git".to_string(), "gh".to_string()]),
            pipeline: Some(vec!["grep".to_string(), "head".to_string()]),
            deny: Some(HashMap::from([(
                "git".to_string(),
                vec!["grep".to_string(), "ls-files".to_string()],
            )])),
            ..CommandsConfig::default()
        };

        let (errors, warnings) = validate(&config);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn validate_client_enforcement_only_with_allow() {
        let config = CommandsConfig {
            client_enforcement_only: true,
            allow: Some(vec!["git".to_string()]),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("client_enforcement_only"));
    }

    #[test]
    fn validate_allow_pipeline_overlap() {
        let config = CommandsConfig {
            allow: Some(vec!["grep".to_string(), "git".to_string()]),
            pipeline: Some(vec!["grep".to_string()]),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("grep"));
        assert!(errors[0].contains("allow"));
        assert!(errors[0].contains("pipeline"));
    }

    #[test]
    fn validate_deny_not_in_allow() {
        let config = CommandsConfig {
            allow: Some(vec!["git".to_string()]),
            deny: Some(HashMap::from([(
                "sqlite3".to_string(),
                vec!["-cmd".to_string()],
            )])),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("sqlite3"));
        assert!(errors[0].contains("not in `allow`"));
    }

    #[test]
    fn validate_empty_allow_entry() {
        let config = CommandsConfig {
            allow: Some(vec!["git".to_string(), String::new()]),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("empty string"));
    }

    #[test]
    fn validate_empty_pipeline_entry() {
        let config = CommandsConfig {
            pipeline: Some(vec![String::new()]),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("empty string"));
    }

    #[test]
    fn validate_empty_deny_subcommand_list() {
        let config = CommandsConfig {
            allow: Some(vec!["git".to_string()]),
            deny: Some(HashMap::from([("git".to_string(), vec![])])),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("empty subcommand list"));
    }

    #[test]
    fn validate_empty_deny_subcommand_string() {
        let config = CommandsConfig {
            allow: Some(vec!["git".to_string()]),
            deny: Some(HashMap::from([("git".to_string(), vec![String::new()])])),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("empty subcommand string"));
    }

    #[test]
    fn validate_empty_build() {
        let config = CommandsConfig {
            build: Some(String::new()),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("build"));
        assert!(errors[0].contains("empty"));
    }

    #[test]
    fn validate_only_client_enforcement_only() {
        let config = CommandsConfig {
            client_enforcement_only: true,
            ..CommandsConfig::default()
        };

        let (errors, warnings) = validate(&config);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    // ── Project config merge tests ─────────────────────────────────

    #[test]
    fn merge_project_allow_replaces_user() {
        let mut user = ResolvedCommands::default();
        user.merge(&CommandsConfig {
            allow: Some(vec!["git".into(), "gh".into(), "cp".into()]),
            ..CommandsConfig::default()
        });

        let root_a = PathBuf::from("/project/a");
        let project_commands = HashMap::from([(
            root_a.clone(),
            CommandsConfig {
                allow: Some(vec!["git".into(), "kubectl".into()]),
                ..CommandsConfig::default()
            },
        )]);

        let merged = user.merge_project_commands(&[root_a], &project_commands);
        assert!(merged.allow.contains("git"));
        assert!(merged.allow.contains("kubectl"));
        assert!(
            !merged.allow.contains("gh"),
            "user allow replaced by project"
        );
        assert!(
            !merged.allow.contains("cp"),
            "user allow replaced by project"
        );
    }

    #[test]
    fn merge_project_multi_root_unions_allow() {
        let mut user = ResolvedCommands::default();
        user.merge(&CommandsConfig {
            allow: Some(vec!["git".into(), "gh".into(), "cp".into()]),
            ..CommandsConfig::default()
        });

        let root_a = PathBuf::from("/project/a");
        let root_b = PathBuf::from("/project/b");
        let project_commands = HashMap::from([
            (
                root_a.clone(),
                CommandsConfig {
                    allow: Some(vec!["git".into(), "kubectl".into()]),
                    ..CommandsConfig::default()
                },
            ),
            // root_b has no commands — falls back to user
        ]);

        let merged = user.merge_project_commands(&[root_a, root_b], &project_commands);
        // Root A: git, kubectl. Root B: git, gh, cp (user fallback). Union:
        assert!(merged.allow.contains("git"));
        assert!(merged.allow.contains("kubectl"));
        assert!(merged.allow.contains("gh"));
        assert!(merged.allow.contains("cp"));
    }

    #[test]
    fn merge_project_disabled_root_contributes_build() {
        let mut user = ResolvedCommands::default();
        user.merge(&CommandsConfig {
            allow: Some(vec!["git".into()]),
            build: Some("make".into()),
            ..CommandsConfig::default()
        });

        let root = PathBuf::from("/disabled/root");
        let project_commands = HashMap::from([(
            root.clone(),
            CommandsConfig {
                build: Some("npm".into()),
                ..CommandsConfig::default()
            },
        )]);

        let merged = user.merge_project_commands(std::slice::from_ref(&root), &project_commands);
        assert_eq!(merged.build.get(&root).map(String::as_str), Some("npm"));
    }

    #[test]
    fn merge_project_build_per_root_with_cwd() {
        let mut user = ResolvedCommands::default();
        user.merge(&CommandsConfig {
            allow: Some(vec!["git".into()]),
            build: Some("make".into()),
            ..CommandsConfig::default()
        });

        let root_a = PathBuf::from("/project/a");
        let root_b = PathBuf::from("/project/b");
        let project_commands = HashMap::from([(
            root_a.clone(),
            CommandsConfig {
                build: Some("npm".into()),
                ..CommandsConfig::default()
            },
        )]);

        let merged = user.merge_project_commands(&[root_a, root_b], &project_commands);
        // Root A: npm (project override). Root B: make (user default).
        assert_eq!(
            merged.build_for_cwd(Some(Path::new("/project/a/src"))),
            Some("npm"),
        );
        assert_eq!(
            merged.build_for_cwd(Some(Path::new("/project/b/lib"))),
            Some("make"),
        );
    }

    #[test]
    fn merge_project_no_roots_returns_clone() {
        let mut user = ResolvedCommands::default();
        user.merge(&CommandsConfig {
            allow: Some(vec!["git".into()]),
            build: Some("make".into()),
            ..CommandsConfig::default()
        });

        let merged = user.merge_project_commands(&[], &HashMap::new());
        assert!(merged.allow.contains("git"));
        assert_eq!(merged.default_build.as_deref(), Some("make"));
        assert!(merged.build.is_empty());
    }

    #[test]
    fn build_for_cwd_falls_back_to_default() {
        let resolved = ResolvedCommands {
            default_build: Some("make".into()),
            ..ResolvedCommands::default()
        };
        // No roots in build map — falls back to default
        assert_eq!(
            resolved.build_for_cwd(Some(Path::new("/any/path"))),
            Some("make")
        );
        assert_eq!(resolved.build_for_cwd(None), Some("make"));
    }

    #[test]
    fn build_for_cwd_longest_prefix_match() {
        let resolved = ResolvedCommands {
            build: HashMap::from([
                (PathBuf::from("/project"), "make".into()),
                (PathBuf::from("/project/nested"), "npm".into()),
            ]),
            ..ResolvedCommands::default()
        };
        assert_eq!(
            resolved.build_for_cwd(Some(Path::new("/project/nested/src"))),
            Some("npm"),
        );
        assert_eq!(
            resolved.build_for_cwd(Some(Path::new("/project/other"))),
            Some("make"),
        );
    }

    #[test]
    fn merge_project_pipeline_replaces_per_root() {
        let mut user = ResolvedCommands::default();
        user.merge(&CommandsConfig {
            allow: Some(vec!["git".into()]),
            pipeline: Some(vec!["grep".into(), "head".into()]),
            ..CommandsConfig::default()
        });

        let root_a = PathBuf::from("/project/a");
        let root_b = PathBuf::from("/project/b");
        let project_commands = HashMap::from([(
            root_a.clone(),
            CommandsConfig {
                pipeline: Some(vec!["jq".into()]),
                ..CommandsConfig::default()
            },
        )]);

        let merged = user.merge_project_commands(&[root_a, root_b], &project_commands);
        // Root A: jq (project). Root B: grep, head (user). Union:
        assert!(merged.pipeline.contains("jq"));
        assert!(merged.pipeline.contains("grep"));
        assert!(merged.pipeline.contains("head"));
    }

    #[test]
    fn merge_project_deny_replaces_per_root() {
        let mut user = ResolvedCommands::default();
        user.merge(&CommandsConfig {
            allow: Some(vec!["git".into()]),
            deny: Some(HashMap::from([("git".into(), vec!["grep".into()])])),
            ..CommandsConfig::default()
        });

        let root_a = PathBuf::from("/project/a");
        let root_b = PathBuf::from("/project/b");
        let project_commands = HashMap::from([(
            root_a.clone(),
            CommandsConfig {
                deny: Some(HashMap::from([("git".into(), vec!["ls-files".into()])])),
                ..CommandsConfig::default()
            },
        )]);

        let merged = user.merge_project_commands(&[root_a, root_b], &project_commands);
        let git_deny = merged.deny.get("git").expect("git deny");
        // Root A: ls-files (project). Root B: grep (user). Union:
        assert!(git_deny.contains("grep"));
        assert!(git_deny.contains("ls-files"));
    }
}

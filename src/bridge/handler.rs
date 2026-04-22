// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Bridge handler that maps MCP tool calls to LSP requests.

use anyhow::{Result, anyhow};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::filesystem_manager::FilesystemManager;

use super::tool_server::ToolServer;
use super::toolbox::Toolbox;
use crate::lsp::LspClientManager;
use crate::lsp::instance_key::InstanceKey;
use crate::mcp::{CallToolResult, Tool, ToolHandler};

/// Checks server health for the given instances and emits one-time
/// state-transition notifications via `tracing`.
///
/// Queries each server's liveness, partitions into alive/dead, and
/// compares against `notified_offline` to emit notifications:
/// - Newly dead servers: `warn!()` with offline message.
/// - Previously-offline servers that recovered: `info!()` with recovery message.
///
/// Notifications flow through `LoggingServer` → `NotificationQueueSink` →
/// `systemMessage` at stationary points (session start, agent stop).
pub(super) async fn check_server_health(
    client_manager: &LspClientManager,
    touched_instances: &[InstanceKey],
    notified_offline: &std::sync::Mutex<HashSet<InstanceKey>>,
) {
    let mut alive = Vec::new();
    let mut dead = Vec::new();

    // Classify each touched instance by readiness (not just process liveness —
    // a stuck server is alive but not ready). Both `Healthy` and `Probing`
    // mean the server is accepting requests — `Probing` just hasn't received
    // a successful response yet.
    let clients = client_manager.clients().await;
    for key in touched_instances {
        let ready = if let Some(c) = clients.get(key) {
            let client = c.lock().await;
            matches!(
                client.lifecycle(),
                crate::lsp::state::ServerLifecycle::Healthy
                    | crate::lsp::state::ServerLifecycle::Probing
            )
        } else {
            false
        };

        if ready {
            alive.push(key.clone());
        } else {
            dead.push(key.clone());
        }
    }

    // Determine state transitions against notified_offline
    let mut notified = notified_offline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // Recovery: previously unavailable, now alive
    for key in &alive {
        if notified.remove(key) {
            tracing::warn!(
                source = "lsp.lifecycle",
                language = key.language_id.as_str(),
                server = key.server.as_str(),
                "Language server back online: {} ({}) \u{2014} \
                 diagnostics and language server enrichment re-enabled for \
                 {} files.",
                key.language_id,
                key.server,
                key.language_id,
            );
        }
    }

    // Unavailable: newly dead or stuck, not yet reported
    for key in &dead {
        if notified.insert(key.clone()) {
            tracing::warn!(
                source = "lsp.lifecycle",
                language = key.language_id.as_str(),
                server = key.server.as_str(),
                "Language server unavailable: {} ({}) \u{2014} \
                 diagnostics unavailable for {} files. \
                 grep and glob still work but without \
                 language server enrichment.",
                key.language_id,
                key.server,
                key.language_id,
            );
        }
    }
}

/// MCP tool call router.
///
/// Routes `tools/call` requests to the appropriate tool server and
/// handles editing lifecycle (`start_editing`/`done_editing`).
/// Implements [`ToolHandler`] to maintain clean dependency direction
/// between the `mcp` (protocol) and `bridge` (application) modules.
pub struct McpRouter {
    toolbox: Arc<Toolbox>,
}

impl McpRouter {
    /// Creates a new `McpRouter` wrapping a shared `Toolbox`.
    #[must_use]
    pub const fn new(toolbox: Arc<Toolbox>) -> Self {
        Self { toolbox }
    }
}

/// Expands a leading `~` or `~/` to the user's home directory.
pub(super) fn expand_tilde(path: &str) -> String {
    if (path == "~" || path.starts_with("~/"))
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}{}", &path[1..]);
    }
    path.to_string()
}

/// Resolves a file path, converting relative paths to absolute using the current working directory.
///
/// Expands a leading `~` to `$HOME` before resolution.
pub(super) fn resolve_path(file: &str) -> Result<PathBuf> {
    let expanded = expand_tilde(file);
    let path = PathBuf::from(&expanded);
    if path.is_absolute() {
        Ok(path)
    } else {
        let cwd = std::env::current_dir()
            .map_err(|e| anyhow!("Failed to get current working directory: {e}"))?;
        Ok(cwd.join(path))
    }
}

/// Makes a file path relative to the owning root, for display.
///
/// Uses [`FilesystemManager::resolve_root`] for longest-prefix matching
/// instead of ad-hoc iteration.
pub(super) fn display_path(file: &str, fs: &FilesystemManager) -> String {
    let path = Path::new(file);
    fs.resolve_root(path).map_or_else(
        || file.to_string(),
        |root| {
            path.strip_prefix(&root).map_or_else(
                |_| file.to_string(),
                |rel| rel.to_string_lossy().to_string(),
            )
        },
    )
}

impl ToolHandler for McpRouter {
    fn list_tools(&self) -> Vec<Tool> {
        vec![
            Tool {
                name: "grep".to_string(),
                title: Some("Grep".to_string()),
                description: Some("Search for a pattern across the workspace. Queries the tree-sitter symbol index and ripgrep in parallel. Returns symbols with structural context and navigation edges.\n\nPATTERN (required)\nRegex pattern. Supports `|` for alternation (e.g., `foo|bar`). Matched against symbol names and file contents.\n\nGLOB\nGlob pattern to narrow the search scope. Only files matching the glob are searched. See the glob tool for pattern syntax.\n\nEXCLUDE\nGlob pattern to exclude from matches.\n\nINCLUDE_GITIGNORED (default: false)\nInclude files ignored by `.gitignore` in the search.\n\nINCLUDE_HIDDEN (default: false)\nInclude hidden files (dotfiles) in the search.\n\nOUTPUT\nOutput fits a fixed character budget to protect your context window. Broad queries produce more matches than the budget can show at full detail, so the tool reduces detail automatically. Narrow your pattern or add a glob to get richer results.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern to search for (supports | for alternation)"
                        },
                        "glob": {
                            "type": "string",
                            "description": "Glob pattern to scope the search (e.g., src/**/*.rs)"
                        },
                        "exclude": {
                            "type": "string",
                            "description": "Glob pattern to exclude from matches"
                        },
                        "include_gitignored": {
                            "type": "boolean",
                            "description": "Include gitignored files (default: false)"
                        },
                        "include_hidden": {
                            "type": "boolean",
                            "description": "Include hidden files (default: false)"
                        }
                    },
                    "required": ["pattern"]
                }),
                annotations: Some(serde_json::json!({
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "idempotentHint": true,
                    "openWorldHint": false
                })),
            },
            Tool {
                name: "glob".to_string(),
                title: Some("Glob".to_string()),
                description: Some("Browse the workspace. Auto-detects intent: file path → symbol outline, directory path → listing with symbols, glob pattern → matching files with symbols. Always shows outline-level symbols (structs, classes, enums, interfaces, modules, constants).".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "A file path, directory path, or glob pattern (e.g., 'src/', 'src/main.rs', '**/*.rs')"
                        }
                    },
                    "required": ["pattern"]
                }),
                annotations: Some(serde_json::json!({
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "idempotentHint": true,
                    "openWorldHint": false
                })),
            },
            Tool {
                name: "start_editing".to_string(),
                title: Some("Start Editing".to_string()),
                description: Some("Enter editing mode. Diagnostics are suppressed on all subsequent Edit/Write calls until done_editing is called. Call this before using Edit.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                }),
                annotations: Some(serde_json::json!({
                    "readOnlyHint": false,
                    "destructiveHint": false,
                    "idempotentHint": true,
                    "openWorldHint": false
                })),
            },
            Tool {
                name: "done_editing".to_string(),
                title: Some("Done Editing".to_string()),
                description: Some("Exit editing mode and return LSP diagnostics for all modified files. Must be called after start_editing before using non-Edit tools.\n\nOutput lists every modified file in one of three categories:\n- Diagnostics: file path followed by indented errors/warnings.\n- Clean: files where the language server found no issues (grouped on one line).\n- N/A: files with no language server coverage (grouped on one line).\n\nFile paths are relative to the workspace root.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                }),
                annotations: Some(serde_json::json!({
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "idempotentHint": false,
                    "openWorldHint": false
                })),
            },
        ]
    }

    fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Value>,
        parent_id: Option<i64>,
    ) -> Result<CallToolResult> {
        // Editing tools: no-op triggers. The PreToolUse hook enters editing
        // mode (start_editing) and the PostToolUse hook exits + runs batch
        // diagnostics (done_editing). The hooks own the state transitions
        // because they have the real agent_id from the host CLI.
        if name == "start_editing" {
            return Ok(CallToolResult::text(
                "editing mode \u{2014} diagnostics deferred until done_editing",
            ));
        }

        if name == "done_editing" {
            let files = self.toolbox.editing.drain_all_and_clear();

            if files.is_empty() {
                return Ok(CallToolResult::text("[clean]"));
            }

            let file_strs: Vec<String> = files
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            let file_refs: Vec<&str> = file_strs.iter().map(String::as_str).collect();
            let entry_id = parent_id.unwrap_or(0);
            let output = self
                .toolbox
                .runtime
                .block_on(self.toolbox.diagnostics.process_files(&file_refs, entry_id));

            return Ok(CallToolResult::text(output));
        }

        // Notify servers about filesystem changes before any LSP interaction.
        self.toolbox
            .runtime
            .block_on(self.toolbox.notify_file_changes());

        // ToolServer dispatch: grep, glob
        let params = arguments.unwrap_or(Value::Null);
        let result = match name {
            "grep" => self
                .toolbox
                .runtime
                .block_on(self.toolbox.grep.execute(&params, parent_id)),
            "glob" => self
                .toolbox
                .runtime
                .block_on(self.toolbox.glob.execute(&params, parent_id)),
            _ => return Err(anyhow!("Unknown tool: {name}")),
        };

        match result {
            Ok(v) => {
                let text = v.as_str().unwrap_or("").to_string();
                Ok(CallToolResult::text(text))
            }
            Err(e) => Err(e),
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
    use crate::bridge::filesystem_manager::FilesystemManager;
    use crate::config::{Config, LanguageConfig, ServerBinding, ServerDef};
    use crate::logging::LoggingServer;
    use crate::lsp::instance_key::{InstanceKey, Scope};
    use std::collections::HashMap;

    const MOCK_LANG: &str = "hK9Qz";

    fn test_logging() -> LoggingServer {
        LoggingServer::new()
    }

    fn test_fs_with_roots(roots: &[&str]) -> Arc<FilesystemManager> {
        let fs = Arc::new(FilesystemManager::new());
        fs.set_roots(roots.iter().map(PathBuf::from).collect());
        fs
    }

    fn mockls_bin() -> PathBuf {
        let test_exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .map(|p| p.join("mockls"));
        test_exe.unwrap_or_else(|| PathBuf::from("mockls"))
    }

    fn single_server_config() -> Config {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG}");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![MOCK_LANG.to_string(), "--workspace-folders".to_string()],
                initialization_options: None,
                settings: None,
                min_severity: None,
                file_patterns: Vec::new(),
                compiled_patterns: Vec::new(),
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_name)],
                ..LanguageConfig::default()
            },
        );
        Config {
            language,
            server,
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
            tools: None,
            resolved_commands: None,
        }
    }

    fn two_server_config() -> Config {
        let bin = mockls_bin();
        let server_a = format!("mockls-{MOCK_LANG}-a");
        let server_b = format!("mockls-{MOCK_LANG}-b");
        let mut server = HashMap::new();
        for name in [&server_a, &server_b] {
            server.insert(
                name.clone(),
                ServerDef {
                    command: bin.to_string_lossy().to_string(),
                    args: vec![MOCK_LANG.to_string(), "--workspace-folders".to_string()],
                    initialization_options: None,
                    settings: None,
                    min_severity: None,
                    file_patterns: Vec::new(),
                    compiled_patterns: Vec::new(),
                },
            );
        }
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_a), ServerBinding::new(server_b)],
                ..LanguageConfig::default()
            },
        );
        Config {
            language,
            server,
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
            tools: None,
            resolved_commands: None,
        }
    }

    // ─── check_server_health tests ────────────────────────────────────

    #[tokio::test]
    async fn test_health_check_multi_server_one_dead() {
        // Two servers for the same language: one spawned (healthy via
        // get_client), one fabricated key (not in client map → dead).
        // Only the dead server should be reported offline.
        let config = two_server_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG)
            .expect("lang config")
            .servers
            .iter()
            .map(|b| b.name.clone())
            .collect();
        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // Spawn a server via get_client (extension fallback: .hK9Qz → config key)
        // and wait for it to become healthy before checking health.
        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG}"));
        manager
            .get_client(&path)
            .await
            .expect("spawn via get_client");
        manager.wait_ready_for_path(&path).await;

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 1);
        let healthy_key: InstanceKey = clients.keys().next().expect("one client").clone();

        // Fabricate a key for the second (unspawned) server.
        let dead_key =
            InstanceKey::new(MOCK_LANG.to_string(), bindings[1].clone(), Scope::Workspace);

        let notified = std::sync::Mutex::new(HashSet::<InstanceKey>::new());

        check_server_health(
            &manager,
            &[healthy_key.clone(), dead_key.clone()],
            &notified,
        )
        .await;

        let set = notified.lock().expect("lock");
        assert!(set.contains(&dead_key), "dead server should be reported");
        assert!(
            !set.contains(&healthy_key),
            "healthy server should NOT be reported"
        );
        assert_eq!(set.len(), 1);
        drop(set);
    }

    #[tokio::test]
    async fn test_health_check_recovery() {
        // Pre-populate notified_offline with a server that is now healthy.
        // check_server_health should remove it from the set.
        let manager = LspClientManager::new(
            single_server_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG}"));
        manager
            .get_client(&path)
            .await
            .expect("spawn via get_client");
        manager.wait_ready_for_path(&path).await;

        let clients = manager.clients().await;
        let healthy_key: InstanceKey = clients.keys().next().expect("one client").clone();

        // Pre-populate: this server was previously offline.
        let notified = std::sync::Mutex::new(HashSet::from([healthy_key.clone()]));

        check_server_health(&manager, std::slice::from_ref(&healthy_key), &notified).await;

        let set = notified.lock().expect("lock");
        assert!(
            !set.contains(&healthy_key),
            "recovered server should be removed from notified_offline"
        );
        assert!(set.is_empty());
        drop(set);
    }

    #[tokio::test]
    async fn test_health_check_both_dead() {
        // Two fabricated keys (neither in client map) → both reported.
        let manager = LspClientManager::new(
            two_server_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let key_a = InstanceKey::new(
            MOCK_LANG.to_string(),
            format!("mockls-{MOCK_LANG}-a"),
            Scope::Workspace,
        );
        let key_b = InstanceKey::new(
            MOCK_LANG.to_string(),
            format!("mockls-{MOCK_LANG}-b"),
            Scope::Workspace,
        );

        let notified = std::sync::Mutex::new(HashSet::<InstanceKey>::new());

        check_server_health(&manager, &[key_a.clone(), key_b.clone()], &notified).await;

        let set = notified.lock().expect("lock");
        assert!(set.contains(&key_a), "first dead server should be reported");
        assert!(
            set.contains(&key_b),
            "second dead server should be reported"
        );
        assert_eq!(set.len(), 2);
        drop(set);
    }

    #[tokio::test]
    async fn test_notified_offline_dedup_by_instance_key() {
        // Calling twice with the same dead key should not produce
        // duplicate entries — the second call is a no-op.
        let manager = LspClientManager::new(
            two_server_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let dead_key = InstanceKey::new(
            MOCK_LANG.to_string(),
            format!("mockls-{MOCK_LANG}-a"),
            Scope::Workspace,
        );

        let notified = std::sync::Mutex::new(HashSet::<InstanceKey>::new());

        // First call: inserts.
        check_server_health(&manager, std::slice::from_ref(&dead_key), &notified).await;
        assert_eq!(notified.lock().expect("lock").len(), 1);

        // Second call: key already present, no duplicate.
        check_server_health(&manager, std::slice::from_ref(&dead_key), &notified).await;
        assert_eq!(notified.lock().expect("lock").len(), 1);
    }
}

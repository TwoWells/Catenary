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
use crate::mcp::{CallToolResult, Tool, ToolHandler};

/// Checks server health for the given languages and emits one-time
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
    touched_servers: &[String],
    notified_offline: &std::sync::Mutex<HashSet<String>>,
) {
    let mut alive = Vec::new();
    let mut dead = Vec::new();

    // Classify each touched server by readiness (not just process liveness —
    // a stuck server is alive but not ready)
    let clients = client_manager.clients().await;
    for lang in touched_servers {
        let ready = if let Some((_, c)) = clients.iter().find(|(k, _)| k.language_id == *lang) {
            let client = c.lock().await;
            matches!(
                client.lifecycle(),
                crate::lsp::state::ServerLifecycle::Healthy
            )
        } else {
            false
        };

        if ready {
            alive.push(lang.clone());
        } else {
            dead.push(lang.clone());
        }
    }

    // Determine state transitions against notified_offline
    let mut notified = notified_offline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // Recovery: previously unavailable, now alive
    for lang in &alive {
        if notified.remove(lang.as_str()) {
            tracing::warn!(
                source = "lsp.lifecycle",
                language = lang.as_str(),
                "Language server back online: {lang} \u{2014} \
                 diagnostics and language server enrichment re-enabled for \
                 {lang} files."
            );
        }
    }

    // Unavailable: newly dead or stuck, not yet reported
    for lang in &dead {
        if notified.insert(lang.clone()) {
            tracing::warn!(
                source = "lsp.lifecycle",
                language = lang.as_str(),
                "Language server unavailable: {lang} \u{2014} \
                 diagnostics unavailable for {lang} files. \
                 grep and glob still work but without \
                 language server enrichment."
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
                description: Some("Exit editing mode and return LSP diagnostics for all modified files. Must be called after start_editing before using non-Edit tools.".to_string()),
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

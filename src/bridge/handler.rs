// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Bridge handler that maps MCP tool calls to LSP requests.

use anyhow::{Result, anyhow};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::warn;

use super::DocumentManager;
use super::diagnostics_server::DiagnosticsServer;
use super::file_tools::GlobServer;
use super::tool_server::ToolServer;
use super::toolbox::Toolbox;
use crate::lsp::{ClientManager, LspClient};
use crate::mcp::{CallToolResult, Tool, ToolContent, ToolHandler};

/// Maximum unique LSP symbols for hover display in grep output.
const GREP_HOVER_THRESHOLD: usize = 10;

/// Result of a server health check against touched language servers.
struct ServerHealth {
    /// Languages with dead servers.
    dead: Vec<String>,
    /// One-time batched notification for state transitions (offline/recovery).
    notification: Option<String>,
}

/// Bridge handler that implements MCP `ToolHandler` trait.
/// Handles MCP tool calls by routing them to the appropriate LSP server.
pub struct LspBridgeHandler {
    toolbox: Toolbox,
    /// Languages whose servers have been reported offline to the agent.
    /// Used for one-time notification: offline is reported once, recovery once.
    notified_offline: std::sync::Mutex<HashSet<String>>,
}

impl LspBridgeHandler {
    /// Creates a new `LspBridgeHandler` wrapping a `Toolbox`.
    pub fn new(
        client_manager: Arc<ClientManager>,
        doc_manager: Arc<Mutex<DocumentManager>>,
        runtime: tokio::runtime::Handle,
        diagnostics: Arc<DiagnosticsServer>,
        session_id: Option<String>,
    ) -> Self {
        let toolbox = Toolbox::new(
            client_manager,
            doc_manager,
            runtime,
            diagnostics,
            session_id,
        );
        Self {
            toolbox,
            notified_offline: std::sync::Mutex::new(HashSet::new()),
        }
    }

    /// Gets the appropriate LSP client for the given file path.
    async fn get_client_for_path(&self, path: &Path) -> Result<Arc<Mutex<LspClient>>> {
        let lang_id = {
            let doc_manager = self.toolbox.doc_manager.lock().await;
            doc_manager.language_id_for_path(path).to_string()
        };

        self.toolbox
            .client_manager
            .get_client_for_path(path, &lang_id)
            .await
    }

    /// Waits for the server handling the given path to be ready.
    ///
    /// Dead servers are non-fatal — the wait completes and the caller
    /// uses [`Self::check_server_health`] to detect the state.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "Client lock held across wait_ready call"
    )]
    async fn wait_for_server_ready(&self, path: &Path) {
        let Ok(client_mutex) = self.get_client_for_path(path).await else {
            return; // No LSP server configured for this language
        };

        let client = client_mutex.lock().await;
        let lang = client.language().to_string();
        let is_ready = client.wait_ready().await;
        drop(client);

        if !is_ready {
            warn!("[{lang}] server died \u{2014} tool will run in degraded mode");
        }
    }

    /// Waits for all active LSP servers to be ready.
    ///
    /// Dead servers are non-fatal — the wait completes for each server
    /// and the caller uses [`Self::check_server_health`] to detect state.
    /// Used for symbol-only queries that don't target a specific file.
    async fn wait_for_all_servers_ready(&self) {
        let clients = self.toolbox.client_manager.clients().await;

        for (lang, client_mutex) in clients {
            if !client_mutex.lock().await.wait_ready().await {
                warn!("[{lang}] server died \u{2014} tool will run in degraded mode");
            }
        }
    }

    /// Checks server health for the given languages and generates one-time
    /// state-transition notifications.
    ///
    /// Queries each server's liveness, partitions into alive/dead, and
    /// compares against `notified_offline` to produce batched notifications:
    /// - Newly dead servers get a single offline message with scope of impact.
    /// - Previously-offline servers that recovered get a single recovery message.
    fn check_server_health(&self, touched_servers: &[String]) -> ServerHealth {
        let mut alive = Vec::new();
        let mut dead = Vec::new();

        // Classify each touched server by readiness (not just process liveness —
        // a stuck server is alive but not ready)
        let clients = self
            .toolbox
            .runtime
            .block_on(self.toolbox.client_manager.clients());
        for lang in touched_servers {
            let ready = clients.get(lang).is_some_and(|c| {
                self.toolbox.runtime.block_on(async {
                    let client = c.lock().await;
                    // Lightweight idle probe: if a stuck server has gone idle,
                    // recover it to Ready before checking readiness.
                    client.try_idle_recover();
                    client.is_ready()
                })
            });

            if ready {
                alive.push(lang.clone());
            } else {
                dead.push(lang.clone());
            }
        }

        // Determine state transitions against notified_offline
        let mut notified = self
            .notified_offline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut parts = Vec::new();

        // Recovery: previously unavailable, now alive
        let recovered: Vec<String> = alive
            .iter()
            .filter(|lang| notified.remove(lang.as_str()))
            .cloned()
            .collect();

        if !recovered.is_empty() {
            let langs = recovered.join(", ");
            parts.push(format!(
                "Language server{} back online: {langs} \u{2014} \
                 diagnostics and language server enrichment re-enabled for \
                 {langs} files.",
                if recovered.len() == 1 { "" } else { "s" },
            ));
        }

        // Unavailable: newly dead or stuck, not yet reported
        let newly_dead: Vec<String> = dead
            .iter()
            .filter(|lang| notified.insert((*lang).clone()))
            .cloned()
            .collect();

        if !newly_dead.is_empty() {
            let langs = newly_dead.join(", ");
            parts.push(format!(
                "Language server{} unavailable: {langs} \u{2014} \
                 diagnostics unavailable for {langs} files. \
                 grep and glob still work but without \
                 language server enrichment.",
                if newly_dead.len() == 1 { "" } else { "s" },
            ));
        }

        let notification = if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        };

        ServerHealth { dead, notification }
    }

    /// Extract file path from arguments if present.
    fn extract_file_path(arguments: Option<&serde_json::Value>) -> Option<PathBuf> {
        arguments
            .and_then(|v| v.get("file"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
    }

    /// Returns the language key for a file path, matching the key used in
    /// `clients()`. This may differ from the LSP language ID for
    /// custom/test languages where the config key is the file extension.
    async fn language_for_path(&self, path: &Path) -> Option<String> {
        let lang_id = {
            let doc_manager = self.toolbox.doc_manager.lock().await;
            doc_manager.language_id_for_path(path).to_string()
        };
        let client_mutex = self
            .toolbox
            .client_manager
            .get_client_for_path(path, &lang_id)
            .await
            .ok()?;
        Some(client_mutex.lock().await.language().to_string())
    }
}

/// Resolves a file path, converting relative paths to absolute using the current working directory.
pub(super) fn resolve_path(file: &str) -> Result<PathBuf> {
    let path = PathBuf::from(file);
    if path.is_absolute() {
        Ok(path)
    } else {
        let cwd = std::env::current_dir()
            .map_err(|e| anyhow!("Failed to get current working directory: {e}"))?;
        Ok(cwd.join(path))
    }
}

/// Makes a file path relative to the nearest root, for display.
pub(super) fn display_path(file: &str, roots: &[PathBuf]) -> String {
    roots
        .iter()
        .find_map(|root| {
            let root_str = root.to_string_lossy();
            file.strip_prefix(root_str.as_ref())
                .map(|rest| rest.strip_prefix('/').unwrap_or(rest).to_string())
        })
        .unwrap_or_else(|| file.to_string())
}

/// Returns `true` if a capability key is present and non-null.
pub(super) fn has_cap(caps: &Value, key: &str) -> bool {
    caps.get(key).is_some_and(|v| !v.is_null())
}

impl ToolHandler for LspBridgeHandler {
    fn list_tools(&self) -> Vec<Tool> {
        vec![
            Tool {
                name: "grep".to_string(),
                description: Some(format!("Search for a pattern across the workspace. Queries the full LSP symbol index and ripgrep in parallel. Use `|` for alternation (e.g., `foo|bar`). Scope with `glob` and `exclude` to narrow the file set. Returns per-symbol sections with definitions, hover docs, and references (\u{2264}{GREP_HOVER_THRESHOLD} symbols) or name+kind+location (>{GREP_HOVER_THRESHOLD}).")),
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
                description: Some("Signal that you intend to make multiple edits to a file. Diagnostics are suppressed until done_editing is called. Call this before using Edit on a file.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file": {
                            "type": "string",
                            "description": "File path to start editing"
                        }
                    },
                    "required": ["file"]
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
                description: Some("Signal that you are finished editing a file. Returns LSP diagnostics for the final state. Must be called after start_editing before using non-Edit tools.".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file": {
                            "type": "string",
                            "description": "File path to finish editing"
                        }
                    },
                    "required": ["file"]
                }),
                annotations: Some(serde_json::json!({
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "idempotentHint": false,
                    "openWorldHint": false
                })),
            },
            Tool {
                name: "replace".to_string(),
                description: Some("Batch replacement across one or more files.\n\nGLOB (required)\n  File path or glob pattern.\n    src/main.rs          single file\n    src/**/*.rs          all Rust files under src/\n    **/*.md              all markdown files\n\n  Directory paths are not accepted \u{2014} use a glob pattern to match\n  files in a directory (e.g., src/bridge/*.rs).\n\nEDITS (required)\n  Array of {old, new, flags?} replacements applied sequentially.\n\n  old      text to find (literal or regex)\n  new      replacement text ($1, $2, ${name} in regex mode)\n  flags    optional:\n             g  replace all occurrences\n             r  treat old as regex, new supports capture groups\n             i  case insensitive (implies r)\n             m  multiline (implies r)\n             s  dotall (implies r)\n\n  No flags = literal match, first occurrence only (same as Edit).\n\n  Examples:\n    { old: \"OldType\", new: \"NewType\", flags: \"g\" }\n    { old: \"use crate::old\", new: \"use crate::new\" }\n\nLINES (optional)\n  Line ranges to constrain replacements. Space-separated.\n    1-10       lines 1 through 10\n    30         just line 30\n    70-        line 70 through EOF\n\nEXCLUDE (optional)\n  Glob pattern to exclude from matches.\n\nINCLUDE_GITIGNORED (default: false)\n  Include gitignored files in glob expansion.\n\nINCLUDE_HIDDEN (default: false)\n  Include hidden files (dotfiles) in glob expansion.\n\nOUTPUT\n  Per-file replacement count with sample diffs. LSP diagnostics\n  (if any) appear after the summary.\n\nSAFETY\n  Every call creates a pre-edit snapshot. Undo with:\n    catenary restore <file>         most recent snapshot\n    catenary restore --id <N>       specific snapshot\n    catenary restore --list         show all snapshots\n  Clean up sidecars: catenary gc --sidecars".to_string()),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "glob": {
                            "type": "string",
                            "description": "File path or glob pattern"
                        },
                        "edits": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old": { "type": "string", "description": "Text to find" },
                                    "new": { "type": "string", "description": "Replacement text" },
                                    "flags": { "type": "string", "description": "Flags: g (global), r (regex), i, m, s" }
                                },
                                "required": ["old", "new"]
                            },
                            "description": "List of edit operations"
                        },
                        "lines": { "type": "string", "description": "Line ranges (e.g., 1-10 30 70-)" },
                        "exclude": { "type": "string", "description": "Glob pattern to exclude" },
                        "include_gitignored": { "type": "boolean" },
                        "include_hidden": { "type": "boolean" }
                    },
                    "required": ["glob", "edits"]
                }),
                annotations: Some(serde_json::json!({
                    "readOnlyHint": false,
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
    ) -> Result<CallToolResult> {
        let file_path = if name == "glob" {
            GlobServer::extract_glob_file_path(arguments.as_ref())
        } else {
            Self::extract_file_path(arguments.as_ref())
        };

        // Editing tools: early dispatch, no LSP readiness wait.
        // start_editing is db-only; done_editing delegates to DiagnosticsServer.
        if name == "start_editing" || name == "done_editing" {
            let file = arguments
                .as_ref()
                .and_then(|v| v.get("file"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("{name} requires a 'file' parameter"))?;

            let roots = self
                .toolbox
                .runtime
                .block_on(self.toolbox.client_manager.roots());

            let result = if name == "start_editing" {
                self.toolbox.editing.start_editing(file, &roots)
            } else {
                self.toolbox
                    .runtime
                    .block_on(self.toolbox.editing.done_editing(file, &roots))
            };

            return match result {
                Ok(text) => Ok(CallToolResult::text(text)),
                Err(e) => Ok(CallToolResult::error(e.to_string())),
            };
        }

        // Replace: early dispatch, no LSP readiness wait — handles its own
        // LSP interaction via DiagnosticsServer after the file write.
        if name == "replace" {
            let params = arguments.unwrap_or(serde_json::Value::Null);
            let result = self
                .toolbox
                .runtime
                .block_on(self.toolbox.replace.execute(&params, None));

            return match result {
                Ok(v) => {
                    let text = v.as_str().unwrap_or("").to_string();
                    Ok(CallToolResult::text(text))
                }
                Err(e) => Err(e),
            };
        }

        // Wait for LSP readiness, then check server health.
        // Dead servers are non-fatal — tools degrade gracefully.
        let health = file_path.as_ref().map_or_else(
            || {
                // Symbol-only: wait for all servers
                self.toolbox
                    .runtime
                    .block_on(self.wait_for_all_servers_ready());
                let touched: Vec<String> = self
                    .toolbox
                    .runtime
                    .block_on(self.toolbox.client_manager.clients())
                    .keys()
                    .cloned()
                    .collect();
                self.check_server_health(&touched)
            },
            |path| {
                // File-scoped: wait for the specific server
                self.toolbox
                    .runtime
                    .block_on(self.wait_for_server_ready(path));
                let touched: Vec<String> = self
                    .toolbox
                    .runtime
                    .block_on(self.language_for_path(path))
                    .into_iter()
                    .collect();
                self.check_server_health(&touched)
            },
        );

        // File-scoped tool with dead server: skip dispatch, return notification
        if !health.dead.is_empty() && file_path.is_some() && name != "glob" {
            let notification = health.notification.unwrap_or_default();
            return Ok(CallToolResult::text(notification));
        }

        // Dispatch tool
        let mut result = match name {
            "grep" => self
                .toolbox
                .grep
                .handle_grep(arguments, &self.toolbox.fs_cache),
            "glob" => self
                .toolbox
                .glob
                .handle_glob(arguments, &self.toolbox.fs_cache),
            _ => Err(anyhow!("Unknown tool: {name}")),
        };

        // Prepend state-transition notification to the result
        if let Some(note) = health.notification
            && let Ok(ref mut res) = result
        {
            res.content.insert(0, ToolContent::Text { text: note });
        }

        result
    }
}

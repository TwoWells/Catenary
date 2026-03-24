// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Bridge handler that maps MCP tool calls to LSP requests.

use anyhow::{Result, anyhow};
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use super::diagnostics_server::DiagnosticsServer;
use super::tool_server::ToolServer;
use super::toolbox::Toolbox;
use crate::lsp::ClientManager;
use crate::mcp::{CallToolResult, Tool, ToolHandler};

/// Maximum unique LSP symbols for hover display in grep output.
const GREP_HOVER_THRESHOLD: usize = 10;

/// Result of a server health check against touched language servers.
#[allow(dead_code, reason = "dead field available for future tool-level use")]
pub(super) struct ServerHealth {
    /// Languages with dead servers.
    pub(super) dead: Vec<String>,
    /// One-time batched notification for state transitions (offline/recovery).
    pub(super) notification: Option<String>,
}

/// Checks server health for the given languages and generates one-time
/// state-transition notifications.
///
/// Queries each server's liveness, partitions into alive/dead, and
/// compares against `notified_offline` to produce batched notifications:
/// - Newly dead servers get a single offline message with scope of impact.
/// - Previously-offline servers that recovered get a single recovery message.
pub(super) async fn check_server_health(
    client_manager: &ClientManager,
    touched_servers: &[String],
    notified_offline: &std::sync::Mutex<HashSet<String>>,
) -> ServerHealth {
    let mut alive = Vec::new();
    let mut dead = Vec::new();

    // Classify each touched server by readiness (not just process liveness —
    // a stuck server is alive but not ready)
    let clients = client_manager.clients().await;
    for lang in touched_servers {
        let ready = if let Some(c) = clients.get(lang) {
            let client = c.lock().await;
            // Lightweight idle probe: if a stuck server has gone idle,
            // recover it to Ready before checking readiness.
            client.try_idle_recover();
            client.is_ready()
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

/// Bridge handler that implements MCP `ToolHandler` trait.
/// Handles MCP tool calls by routing them to the appropriate LSP server.
pub struct LspBridgeHandler {
    toolbox: Toolbox,
}

impl LspBridgeHandler {
    /// Creates a new `LspBridgeHandler` wrapping a `Toolbox`.
    pub fn new(
        client_manager: Arc<ClientManager>,
        doc_manager: Arc<tokio::sync::Mutex<super::DocumentManager>>,
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
        Self { toolbox }
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
        ]
    }

    fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Value>,
        parent_id: Option<i64>,
    ) -> Result<CallToolResult> {
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

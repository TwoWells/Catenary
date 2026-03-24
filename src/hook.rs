// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! IPC server for host CLI hook integration.
//!
//! `HookServer` is the protocol boundary for all hook traffic, same as
//! `McpServer` is for MCP and `Connection`/`LspServer` is for LSP. All
//! hook logic runs server-side. CLI hook processes are dumb transports:
//! read stdin from the host CLI, connect to IPC, forward the request,
//! format the response for the host.
//!
//! Hook methods are caller-supplied and follow the `namespace/action`
//! convention used by MCP (`tools/call`) and LSP (`textDocument/hover`).
//!
//! Transport: Unix domain sockets on Unix, named pipes on Windows.

use anyhow::{Result, anyhow};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixListener;
use tracing::{debug, info, warn};

use crate::bridge::toolbox::Toolbox;
use crate::db;
use crate::session::MessageLog;

/// IPC request from the CLI hook process to the hook server.
///
/// Dispatched by the `method` field. Each variant corresponds to one of the
/// five host CLI hooks.
#[derive(Debug, Deserialize)]
#[serde(tag = "method")]
enum HookRequest {
    /// Refresh workspace roots via MCP `roots/list`.
    #[serde(rename = "pre-agent/roots-sync")]
    PreAgentRootsSync {},

    /// Editing state enforcement: deny or allow a tool call.
    #[serde(rename = "pre-tool/enforce-editing")]
    PreToolEnforceEditing {
        /// Host CLI tool name (e.g., "Edit", "Write", `"write_file"`).
        tool_name: String,
        /// Absolute path to the target file (unused in stateless API but
        /// preserved for backwards compatibility with hook callers).
        #[serde(default)]
        #[allow(dead_code, reason = "kept for IPC backwards compatibility")]
        file_path: Option<String>,
        /// Agent ID (empty string for the main agent).
        #[serde(default)]
        agent_id: String,
        /// Host CLI session ID (Claude Code / Gemini CLI UUID).
        #[serde(default)]
        session_id: Option<String>,
    },

    /// LSP diagnostics for a changed file.
    #[serde(rename = "post-tool/diagnostics")]
    PostToolDiagnostics {
        /// Absolute path to the changed file.
        file: String,
        /// Name of the host CLI tool that triggered the hook.
        /// Used for file accumulation during editing mode and logged
        /// in the payload for monitor visibility.
        #[serde(default)]
        tool: Option<String>,
        /// Agent ID (empty string for the main agent).
        #[serde(default)]
        agent_id: String,
        /// Host CLI session ID (Claude Code / Gemini CLI UUID).
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Force `done_editing` before the agent stops.
    #[serde(rename = "post-agent/require-release")]
    PostAgentRequireRelease {
        /// Agent ID (empty string for the main agent).
        #[serde(default)]
        agent_id: String,
        /// Whether this is a retry (Claude Code `stop_hook_active`).
        #[serde(default)]
        stop_hook_active: bool,
    },

    /// Clear stale editing state on session start.
    #[serde(rename = "session-start/clear-editing")]
    SessionStartClearEditing {
        /// Host CLI session ID (Claude Code / Gemini CLI UUID).
        #[serde(default)]
        session_id: Option<String>,
    },
}

/// IPC response from the hook server to the CLI.
///
/// Handlers return `Option<HookResult>`: `None` means "allow" (empty
/// response — CLI outputs nothing). Variants carry actionable data
/// for the CLI to format for the host.
#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookResult {
    /// Diagnostic content for the model (may be `[clean]`, `[no language server]`,
    /// `[diagnostics unavailable]`, or formatted diagnostic lines).
    Content(String),
    /// Internal error for the user (path resolution, LSP client failures, etc.).
    Error(String),
    /// Deny with reason (pre-tool enforcement).
    Deny(String),
    /// Block with reason (post-agent enforcement).
    Block(String),
    /// Diagnostic content with courtesy notice (another agent is editing).
    Courtesy(String),
    /// Cleared editing state entries.
    Cleared(usize),
}

// ── Tool classification helpers ─────────────────────────────────────────

/// Returns `true` if the tool is an edit tool that requires `start_editing`.
///
/// Checks all known edit tool names across host CLIs (Claude Code and Gemini CLI).
fn is_edit_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "Edit" | "Write" | "NotebookEdit" | "write_file" | "replace"
    )
}

/// Returns `true` if the tool is a read tool (always allowed during editing).
///
/// Checks all known read tool names across host CLIs.
fn is_read_tool(tool_name: &str) -> bool {
    matches!(tool_name, "Read" | "NotebookRead" | "read_file")
}

/// Returns `true` if the tool is always allowed during editing mode.
///
/// Catenary editing tools (`start_editing`, `done_editing`) must be allowed
/// so the agent can manage editing state. `ToolSearch` must be allowed
/// because both editing tools are deferred in Claude Code — blocking
/// `ToolSearch` while editing creates an unrecoverable state if the agent
/// loaded `start_editing` but not `done_editing` before entering editing mode.
fn is_allowed_during_editing(tool_name: &str) -> bool {
    tool_name.contains("start_editing")
        || tool_name.contains("done_editing")
        || tool_name == "ToolSearch"
}

// ── HookServer ──────────────────────────────────────────────────────────

/// Listens on an IPC endpoint for hook requests from the host CLI.
///
/// Protocol boundary for all hook traffic. Dispatches on caller-supplied
/// method names, runs editing state logic, diagnostics, and root sync
/// server-side, and logs all hook messages for monitor visibility.
pub struct HookServer {
    toolbox: Arc<Toolbox>,
    refresh_roots: Arc<AtomicBool>,
    message_log: Arc<MessageLog>,
    conn: Arc<Mutex<Connection>>,
    session_id: String,
    client_name: String,
}

impl HookServer {
    /// Creates a new `HookServer`.
    #[must_use]
    pub const fn new(
        toolbox: Arc<Toolbox>,
        refresh_roots: Arc<AtomicBool>,
        message_log: Arc<MessageLog>,
        conn: Arc<Mutex<Connection>>,
        session_id: String,
        client_name: String,
    ) -> Self {
        Self {
            toolbox,
            refresh_roots,
            message_log,
            conn,
            session_id,
            client_name,
        }
    }

    /// Starts listening on the given IPC endpoint.
    ///
    /// Spawns a background task that accepts connections and processes
    /// hook requests. Returns a `JoinHandle` for the listener task.
    ///
    /// # Errors
    ///
    /// Returns an error if the endpoint cannot be created.
    #[cfg(unix)]
    pub fn start(self, socket_path: &std::path::Path) -> Result<tokio::task::JoinHandle<()>> {
        // Remove stale socket file if it exists
        let _ = std::fs::remove_file(socket_path);

        let listener = UnixListener::bind(socket_path).map_err(|e| {
            anyhow!(
                "Failed to bind notify socket {}: {e}",
                socket_path.display()
            )
        })?;

        info!("Notify socket listening on {}", socket_path.display());

        let server = Arc::new(self);

        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let server = server.clone();
                        tokio::spawn(async move {
                            if let Err(e) = server.handle_connection(stream).await {
                                debug!("Notify connection error: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        warn!("Notify socket accept error: {e}");
                    }
                }
            }
        });

        Ok(handle)
    }

    /// Starts listening on the given named pipe path.
    ///
    /// Spawns a background task that accepts connections and processes
    /// hook requests. Returns a `JoinHandle` for the listener task.
    ///
    /// # Errors
    ///
    /// Returns an error if the named pipe cannot be created.
    #[cfg(windows)]
    pub fn start(self, pipe_path: &std::path::Path) -> Result<tokio::task::JoinHandle<()>> {
        use tokio::net::windows::named_pipe::ServerOptions;

        let pipe_name = pipe_path.to_string_lossy().to_string();

        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)
            .map_err(|e| anyhow!("Failed to create notify pipe {pipe_name}: {e}"))?;

        info!("Notify pipe listening on {pipe_name}");

        let server_arc = Arc::new(self);

        let handle = tokio::spawn(async move {
            loop {
                // Wait for a client to connect to the current instance
                if let Err(e) = server.connect().await {
                    warn!("Notify pipe connect error: {e}");
                    continue;
                }

                let connected = server;

                // Create a fresh pipe instance before spawning the handler
                // so clients never see ERROR_FILE_NOT_FOUND
                server = match ServerOptions::new().create(&pipe_name) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("Notify pipe create error: {e}");
                        break;
                    }
                };

                let srv = server_arc.clone();
                tokio::spawn(async move {
                    if let Err(e) = srv.handle_connection(connected).await {
                        debug!("Notify connection error: {e}");
                    }
                });
            }
        });

        Ok(handle)
    }

    /// Handles a single connection: reads a JSON request, extracts the method
    /// string, dispatches to the appropriate handler, logs both request and
    /// response, and writes back the result.
    async fn handle_connection<S: AsyncRead + AsyncWrite + Unpin>(&self, stream: S) -> Result<()> {
        let (reader, mut writer) = tokio::io::split(stream);
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        buf_reader.read_line(&mut line).await?;

        let raw: Value =
            serde_json::from_str(line.trim()).map_err(|e| anyhow!("Invalid request: {e}"))?;
        let method = raw
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        // Log incoming hook request
        let entry_id = self.message_log.log(
            "hook",
            &method,
            "catenary",
            &self.client_name,
            None,
            None,
            &raw,
        );

        let request: HookRequest =
            serde_json::from_value(raw).map_err(|e| anyhow!("Invalid hook request: {e}"))?;

        let result = match request {
            HookRequest::PreAgentRootsSync {} => {
                debug!("Hook: refresh roots requested");
                self.refresh_roots.store(true, Ordering::Release);
                None
            }
            HookRequest::PreToolEnforceEditing {
                tool_name,
                file_path: _,
                agent_id,
                session_id,
            } => {
                self.store_client_session_id(session_id.as_deref());
                self.handle_enforce_editing(&tool_name, &agent_id)
            }
            HookRequest::PostToolDiagnostics {
                file,
                tool,
                agent_id,
                session_id,
            } => {
                self.store_client_session_id(session_id.as_deref());
                debug!("Hook: processing file {file}");
                self.handle_diagnostics(&file, &agent_id, tool.as_deref(), entry_id)
                    .await
            }
            HookRequest::PostAgentRequireRelease {
                agent_id,
                stop_hook_active,
            } => self.handle_require_release(&agent_id, stop_hook_active),
            HookRequest::SessionStartClearEditing { session_id } => {
                self.store_client_session_id(session_id.as_deref());
                self.handle_clear_editing()
            }
        };

        let response = result
            .as_ref()
            .map(|r| serde_json::to_string(r).unwrap_or_default())
            .unwrap_or_default();

        // Log outgoing hook response
        self.message_log.log(
            "hook",
            &method,
            "catenary",
            &self.client_name,
            Some(entry_id),
            None,
            &serde_json::from_str::<Value>(&response).unwrap_or_default(),
        );

        writer.write_all(response.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.shutdown().await?;

        Ok(())
    }

    // ── Hook handlers ───────────────────────────────────────────────────

    /// Editing state enforcement: deny or allow a tool call.
    ///
    /// If the agent is in editing mode, only Edit/Read/Write and Catenary
    /// editing tools are allowed. If the agent is not in editing mode,
    /// Edit/Write requires `start_editing` first.
    fn handle_enforce_editing(&self, tool_name: &str, agent_id: &str) -> Option<HookResult> {
        let agent_editing = self
            .conn
            .lock()
            .ok()
            .and_then(|c| db::is_agent_editing(&c, &self.session_id, agent_id).ok())
            .unwrap_or(false);

        if agent_editing {
            if is_allowed_during_editing(tool_name)
                || is_read_tool(tool_name)
                || is_edit_tool(tool_name)
            {
                None
            } else {
                Some(HookResult::Deny(
                    "call done_editing to get diagnostics".into(),
                ))
            }
        } else if is_edit_tool(tool_name) {
            Some(HookResult::Deny("call start_editing before editing".into()))
        } else {
            None
        }
    }

    /// LSP diagnostics for a changed file, with editing state checks.
    ///
    /// When the agent is in editing mode: accumulates edit-tool file paths
    /// and suppresses diagnostics (returns `None`). Adds a courtesy flag
    /// when another agent has the file in their accumulated files.
    async fn handle_diagnostics(
        &self,
        file_path: &str,
        agent_id: &str,
        tool_name: Option<&str>,
        entry_id: i64,
    ) -> Option<HookResult> {
        // Check editing state before running diagnostics
        let (agent_editing, other_editing) = {
            let conn = self.conn.lock().ok();
            let self_ed = conn
                .as_ref()
                .and_then(|c| db::is_agent_editing(c, &self.session_id, agent_id).ok())
                .unwrap_or(false);
            let other_ed = conn
                .as_ref()
                .and_then(|c| {
                    db::is_file_edited_by_others(c, file_path, &self.session_id, agent_id).ok()
                })
                .unwrap_or(false);
            drop(conn);
            (self_ed, other_ed)
        };

        if agent_editing {
            // Accumulate edit-tool file paths for done_editing
            if tool_name.is_some_and(is_edit_tool)
                && let Ok(conn) = self.conn.lock()
            {
                let _ = db::add_editing_file(&conn, &self.session_id, agent_id, file_path);
            }
            return None;
        }

        match self
            .toolbox
            .diagnostics
            .process_file(file_path, entry_id)
            .await
        {
            Ok(diag_result) => {
                if other_editing {
                    Some(HookResult::Courtesy(diag_result.content))
                } else {
                    Some(HookResult::Content(diag_result.content))
                }
            }
            Err(e) => {
                warn!("Notify error for {file_path}: {e}");
                Some(HookResult::Error(e.to_string()))
            }
        }
    }

    /// Force `done_editing` before the agent stops.
    ///
    /// If `stop_hook_active` is true (retry), allows unconditionally.
    /// Otherwise blocks if the agent is in editing mode.
    fn handle_require_release(&self, agent_id: &str, stop_hook_active: bool) -> Option<HookResult> {
        if stop_hook_active {
            return None;
        }

        let agent_editing = self
            .conn
            .lock()
            .ok()
            .and_then(|c| db::is_agent_editing(&c, &self.session_id, agent_id).ok())
            .unwrap_or(false);

        if agent_editing {
            Some(HookResult::Block(
                "call done_editing to get diagnostics before finishing".into(),
            ))
        } else {
            None
        }
    }

    /// Clear stale editing state on session start/resume.
    ///
    /// Returns the count of cleared entries, or `None` if nothing was cleared.
    fn handle_clear_editing(&self) -> Option<HookResult> {
        let count = self
            .conn
            .lock()
            .ok()
            .and_then(|c| db::clear_session_editing(&c, &self.session_id).ok())
            .unwrap_or(0);

        if count > 0 {
            Some(HookResult::Cleared(count))
        } else {
            None
        }
    }

    /// Store the host CLI's session ID (idempotent — first write wins).
    fn store_client_session_id(&self, client_session_id: Option<&str>) {
        if let Some(client_sid) = client_session_id
            && let Ok(c) = self.conn.lock()
        {
            let _ = c.execute(
                "UPDATE sessions SET client_session_id = ?1 \
                 WHERE id = ?2 AND client_session_id IS NULL",
                rusqlite::params![client_sid, &self.session_id],
            );
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

    // ── Serialization tests ─────────────────────────────────────────────

    #[test]
    fn hook_result_content_round_trip() {
        let original = HookResult::Content("[clean]".into());
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: HookResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);

        let raw: serde_json::Value = serde_json::from_str(&json).expect("parse as value");
        assert_eq!(raw["content"], "[clean]");
        assert!(raw.get("error").is_none());
    }

    #[test]
    fn hook_result_error_round_trip() {
        let original = HookResult::Error("path resolution failed".into());
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: HookResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);

        let raw: serde_json::Value = serde_json::from_str(&json).expect("parse as value");
        assert_eq!(raw["error"], "path resolution failed");
        assert!(raw.get("content").is_none());
    }

    #[test]
    fn hook_result_deny_round_trip() {
        let original = HookResult::Deny("call start_editing first".into());
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: HookResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);
    }

    #[test]
    fn hook_result_block_round_trip() {
        let original = HookResult::Block("call done_editing first".into());
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: HookResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);
    }

    #[test]
    fn hook_result_courtesy_round_trip() {
        let original = HookResult::Courtesy("[clean]".into());
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: HookResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);
    }

    #[test]
    fn hook_result_cleared_round_trip() {
        let original = HookResult::Cleared(3);
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: HookResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);
    }

    // ── Request deserialization tests ────────────────────────────────────

    #[test]
    fn test_hook_request_tagged_deserialization() {
        // pre-agent/roots-sync
        let json = r#"{"method": "pre-agent/roots-sync"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("roots-sync");
        assert!(matches!(req, HookRequest::PreAgentRootsSync {}));

        // pre-tool/enforce-editing with all fields
        let json = r#"{"method": "pre-tool/enforce-editing", "tool_name": "Edit", "file_path": "/tmp/foo.rs", "agent_id": "", "session_id": "abc123"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("enforce-editing");
        let HookRequest::PreToolEnforceEditing {
            tool_name,
            file_path,
            agent_id,
            session_id,
        } = req
        else {
            unreachable!("expected PreToolEnforceEditing");
        };
        assert_eq!(tool_name, "Edit");
        assert_eq!(file_path.as_deref(), Some("/tmp/foo.rs"));
        assert_eq!(agent_id, "");
        assert_eq!(session_id.as_deref(), Some("abc123"));

        // post-tool/diagnostics with optional fields
        let json =
            r#"{"method": "post-tool/diagnostics", "file": "/tmp/test.rs", "tool": "Write"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("diagnostics");
        let HookRequest::PostToolDiagnostics { file, tool, .. } = req else {
            unreachable!("expected PostToolDiagnostics");
        };
        assert_eq!(file, "/tmp/test.rs");
        assert_eq!(tool.as_deref(), Some("Write"));

        // post-tool/diagnostics without optional fields
        let json = r#"{"method": "post-tool/diagnostics", "file": "/tmp/test.rs"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("diagnostics minimal");
        let HookRequest::PostToolDiagnostics { tool, .. } = req else {
            unreachable!("expected PostToolDiagnostics");
        };
        assert!(tool.is_none());

        // post-agent/require-release
        let json =
            r#"{"method": "post-agent/require-release", "agent_id": "", "stop_hook_active": true}"#;
        let req: HookRequest = serde_json::from_str(json).expect("require-release");
        let HookRequest::PostAgentRequireRelease {
            stop_hook_active, ..
        } = req
        else {
            unreachable!("expected PostAgentRequireRelease");
        };
        assert!(stop_hook_active);

        // session-start/clear-editing
        let json = r#"{"method": "session-start/clear-editing", "session_id": "uuid-123"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("clear-editing");
        let HookRequest::SessionStartClearEditing { session_id } = req else {
            unreachable!("expected SessionStartClearEditing");
        };
        assert_eq!(session_id.as_deref(), Some("uuid-123"));
    }

    // ── Logging tests ───────────────────────────────────────────────────

    #[test]
    fn test_hook_log_file_request() {
        let (_dir, _path, conn) = test_db();
        let conn = Arc::new(std::sync::Mutex::new(conn));

        // Insert a session for the FK.
        conn.lock()
            .expect("lock")
            .execute(
                "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert session");

        let log = Arc::new(MessageLog::new(conn.clone(), "s1".to_string()));

        // Simulate what handle_connection does for a PostToolDiagnostics request
        let method = "post-tool/diagnostics";
        let request_payload = serde_json::json!({
            "method": "post-tool/diagnostics",
            "file": "/tmp/test.rs",
            "tool": "Write"
        });
        let entry_id = log.log(
            "hook",
            method,
            "catenary",
            "claude-code",
            None,
            None,
            &request_payload,
        );
        assert!(entry_id > 0);

        let response_payload = serde_json::json!({"content": "[clean]"});
        let resp_id = log.log(
            "hook",
            method,
            "catenary",
            "claude-code",
            Some(entry_id),
            None,
            &response_payload,
        );
        assert!(resp_id > entry_id);

        // Verify both messages in the database
        let (r_type, r_method): (String, String) = conn
            .lock()
            .expect("lock")
            .query_row(
                "SELECT type, method FROM messages WHERE id = ?1",
                [entry_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("query request");
        assert_eq!(r_type, "hook");
        assert_eq!(r_method, "post-tool/diagnostics");

        let stored_req_id: Option<i64> = conn
            .lock()
            .expect("lock")
            .query_row(
                "SELECT request_id FROM messages WHERE id = ?1",
                [resp_id],
                |row| row.get(0),
            )
            .expect("query response");
        assert_eq!(stored_req_id, Some(entry_id));
    }

    #[test]
    fn test_hook_log_refresh_roots() {
        let (_dir, _path, conn) = test_db();
        let conn = Arc::new(std::sync::Mutex::new(conn));

        conn.lock()
            .expect("lock")
            .execute(
                "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert session");

        let log = Arc::new(MessageLog::new(conn.clone(), "s1".to_string()));

        let method = "pre-agent/roots-sync";
        let request_payload = serde_json::json!({"method": "pre-agent/roots-sync"});
        let entry_id = log.log(
            "hook",
            method,
            "catenary",
            "host",
            None,
            None,
            &request_payload,
        );

        let response_payload = serde_json::json!("");
        log.log(
            "hook",
            method,
            "catenary",
            "host",
            Some(entry_id),
            None,
            &response_payload,
        );

        // Verify method is pre-agent/roots-sync
        let r_method: String = conn
            .lock()
            .expect("lock")
            .query_row(
                "SELECT method FROM messages WHERE id = ?1",
                [entry_id],
                |row| row.get(0),
            )
            .expect("query");
        assert_eq!(r_method, "pre-agent/roots-sync");
    }

    // ── Tool classification tests ───────────────────────────────────────

    #[test]
    fn test_is_edit_tool() {
        // Claude Code edit tools
        assert!(is_edit_tool("Edit"));
        assert!(is_edit_tool("Write"));
        assert!(is_edit_tool("NotebookEdit"));
        // Gemini CLI edit tools
        assert!(is_edit_tool("write_file"));
        assert!(is_edit_tool("replace"));
        // Non-edit tools
        assert!(!is_edit_tool("Read"));
        assert!(!is_edit_tool("Bash"));
        assert!(!is_edit_tool("grep"));
    }

    #[test]
    fn test_is_read_tool() {
        assert!(is_read_tool("Read"));
        assert!(is_read_tool("NotebookRead"));
        assert!(is_read_tool("read_file"));
        assert!(!is_read_tool("Edit"));
        assert!(!is_read_tool("Bash"));
    }

    #[test]
    fn test_is_allowed_during_editing() {
        assert!(is_allowed_during_editing("start_editing"));
        assert!(is_allowed_during_editing("done_editing"));
        assert!(is_allowed_during_editing("ToolSearch"));
        // MCP qualified names
        assert!(is_allowed_during_editing("mcp__catenary__start_editing"));
        assert!(is_allowed_during_editing("mcp__catenary__done_editing"));
        assert!(!is_allowed_during_editing("Edit"));
        assert!(!is_allowed_during_editing("Bash"));
    }

    // ── Handler tests ───────────────────────────────────────────────────

    #[test]
    fn test_hook_enforce_editing_deny() {
        let server = test_server();
        // No editing state — Edit should be denied
        let result = server.handle_enforce_editing("Edit", "");
        let Some(HookResult::Deny(reason)) = result else {
            unreachable!("expected Deny, got {result:?}");
        };
        assert!(reason.contains("start_editing"));
    }

    #[test]
    fn test_hook_enforce_editing_allow() {
        let server = test_server();
        // Enter editing mode
        server
            .conn
            .lock()
            .expect("lock")
            .execute(
                "INSERT INTO editing_state (session_id, agent_id, started_at) \
                 VALUES ('test-session', '', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert editing state");

        // Edit tool — should allow during editing mode
        let result = server.handle_enforce_editing("Edit", "");
        assert!(result.is_none(), "expected allow, got {result:?}");

        // Read tool — always allowed during editing
        let result = server.handle_enforce_editing("Read", "");
        assert!(result.is_none(), "expected allow for Read, got {result:?}");

        // Non-edit, non-read tool while editing — should deny
        let result = server.handle_enforce_editing("Bash", "");
        let Some(HookResult::Deny(reason)) = result else {
            unreachable!("expected Deny for Bash, got {result:?}");
        };
        assert!(reason.contains("done_editing"));
    }

    #[test]
    fn test_hook_require_release_block() {
        let server = test_server();
        // Enter editing mode
        server
            .conn
            .lock()
            .expect("lock")
            .execute(
                "INSERT INTO editing_state (session_id, agent_id, started_at) \
                 VALUES ('test-session', '', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert editing state");

        let result = server.handle_require_release("", false);
        let Some(HookResult::Block(reason)) = result else {
            unreachable!("expected Block, got {result:?}");
        };
        assert!(reason.contains("done_editing"));
    }

    #[test]
    fn test_hook_require_release_allow() {
        let server = test_server();
        // No editing state — should allow
        let result = server.handle_require_release("", false);
        assert!(result.is_none(), "expected allow, got {result:?}");
    }

    #[test]
    fn test_hook_require_release_retry() {
        let server = test_server();
        // Enter editing mode
        server
            .conn
            .lock()
            .expect("lock")
            .execute(
                "INSERT INTO editing_state (session_id, agent_id, started_at) \
                 VALUES ('test-session', '', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert editing state");

        // stop_hook_active = true → always allow regardless of state
        let result = server.handle_require_release("", true);
        assert!(result.is_none(), "expected allow on retry, got {result:?}");
    }

    #[test]
    fn test_hook_clear_editing() {
        let server = test_server();
        // Enter editing mode for two agents
        {
            let c = server.conn.lock().expect("lock");
            c.execute(
                "INSERT INTO editing_state (session_id, agent_id, started_at) \
                 VALUES ('test-session', '', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert agent a");
            c.execute(
                "INSERT INTO editing_state (session_id, agent_id, started_at) \
                 VALUES ('test-session', 'agent-b', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert agent b");
        }

        let result = server.handle_clear_editing();
        assert_eq!(result, Some(HookResult::Cleared(2)));

        // Second call should return None (nothing to clear)
        let result = server.handle_clear_editing();
        assert!(
            result.is_none(),
            "expected None after clear, got {result:?}"
        );
    }

    // ── Test helpers ────────────────────────────────────────────────────

    /// Open an isolated test database in a tempdir.
    fn test_db() -> (tempfile::TempDir, std::path::PathBuf, rusqlite::Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catenary").join("catenary.db");
        let conn = crate::db::open_and_migrate_at(&path).expect("open test DB");
        (dir, path, conn)
    }

    /// Create a `HookServer` with a test database for handler unit tests.
    ///
    /// Uses noop `MessageLog` and minimal dependencies — only `conn` and
    /// `session_id` are exercised by editing state handlers.
    fn test_server() -> TestHookServer {
        let (dir, _path, conn) = test_db();
        let conn = Arc::new(std::sync::Mutex::new(conn));

        // Insert a session for FK constraints.
        conn.lock()
            .expect("lock")
            .execute(
                "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('test-session', 1, 'test', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert session");

        let message_log = Arc::new(MessageLog::noop());
        let config: crate::config::Config = serde_json::from_str("{}").expect("empty config");

        // Toolbox requires a tokio runtime handle for async dispatch.
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let handle = runtime.handle().clone();

        let client_manager = Arc::new(crate::lsp::ClientManager::new(
            config,
            vec![],
            message_log.clone(),
        ));
        let doc_manager = Arc::new(tokio::sync::Mutex::new(
            crate::bridge::DocumentManager::new(String::new()),
        ));
        let path_validator = Arc::new(tokio::sync::RwLock::new(crate::bridge::PathValidator::new(
            vec![],
        )));
        let diagnostics = Arc::new(crate::bridge::DiagnosticsServer::new(
            client_manager.clone(),
            doc_manager.clone(),
            path_validator,
        ));
        let toolbox = Arc::new(Toolbox::new(
            client_manager,
            doc_manager,
            handle,
            diagnostics,
        ));
        let refresh_roots = Arc::new(AtomicBool::new(false));

        let server = HookServer::new(
            toolbox,
            refresh_roots,
            message_log,
            conn,
            "test-session".to_string(),
            "test".to_string(),
        );

        TestHookServer {
            _dir: dir,
            _runtime: runtime,
            server,
        }
    }

    /// Wrapper that keeps the tempdir and runtime alive for the lifetime of the server.
    struct TestHookServer {
        _dir: tempfile::TempDir,
        _runtime: tokio::runtime::Runtime,
        server: HookServer,
    }

    impl std::ops::Deref for TestHookServer {
        type Target = HookServer;
        fn deref(&self) -> &Self::Target {
            &self.server
        }
    }
}

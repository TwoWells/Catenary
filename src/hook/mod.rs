// SPDX-License-Identifier: AGPL-3.0-or-later
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

pub mod response;

use anyhow::{Result, anyhow};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixListener;
use tracing::{debug, info, warn};

use crate::bridge::HookRouter;
use crate::bridge::toolbox::Toolbox;
use crate::session::MessageLog;

/// IPC request from the CLI hook process to the hook server.
///
/// Dispatched by the `method` field. Each variant corresponds to one of the
/// five host CLI hooks.
#[derive(Debug, Deserialize)]
#[serde(tag = "method")]
pub(crate) enum HookRequest {
    /// Refresh workspace roots via MCP `roots/list`.
    #[serde(rename = "pre-agent/roots-sync")]
    PreAgentRootsSync {},

    /// Editing state enforcement: deny or allow a tool call.
    #[serde(rename = "pre-tool/enforce-editing")]
    PreToolEnforceEditing {
        /// Host CLI tool name (e.g., "Edit", "Write", `"write_file"`).
        tool_name: String,
        /// Absolute path to the target file. Used for scope boundary
        /// checks — edits on files outside workspace roots skip the
        /// `start_editing` gate.
        #[serde(default)]
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

/// IPC response envelope carrying both the handler result and an optional
/// `systemMessage` for the user.
///
/// The notification queue is drained at stationary hook points (`SessionStart`,
/// `Stop`/`AfterAgent` when allowing) and delivered as `system_message`. The CLI
/// hook process embeds this string in the host-specific `systemMessage` JSON
/// field.
#[derive(serde::Serialize, serde::Deserialize, Debug, Default, PartialEq, Eq)]
pub struct HookResponseEnvelope {
    /// Handler result (`None` = allow / no actionable data).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<HookResult>,
    /// Composed `systemMessage` content from direct messages and background
    /// notification drain. `None` = no `systemMessage` field in host output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_message: Option<String>,
}

// ── HookServer ──────────────────────────────────────────────────────────

/// Listens on an IPC endpoint for hook requests from the host CLI.
///
/// Protocol boundary for all hook traffic. Parses IPC messages, logs
/// request/response pairs for monitor visibility, and delegates application
/// dispatch to [`HookRouter`].
pub struct HookServer {
    router: Arc<HookRouter>,
    message_log: Arc<MessageLog>,
}

impl HookServer {
    /// Creates a new `HookServer`.
    #[must_use]
    pub fn new(
        toolbox: Arc<Toolbox>,
        refresh_roots: Arc<AtomicBool>,
        message_log: Arc<MessageLog>,
        conn: Arc<Mutex<Connection>>,
        instance_id: String,
        client_name: String,
    ) -> Self {
        let router = Arc::new(HookRouter::new(
            toolbox,
            refresh_roots,
            conn,
            instance_id,
            client_name,
        ));
        Self {
            router,
            message_log,
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
            &self.router.client_name,
            None,
            None,
            &raw,
        );

        let request: HookRequest =
            serde_json::from_value(raw).map_err(|e| anyhow!("Invalid hook request: {e}"))?;

        let result = self.router.dispatch(request, entry_id);

        let envelope = HookResponseEnvelope {
            result: result.result,
            system_message: result.system_message,
        };
        let response = if envelope.result.is_some() || envelope.system_message.is_some() {
            serde_json::to_string(&envelope).unwrap_or_default()
        } else {
            String::new()
        };

        // Log outgoing hook response
        self.message_log.log(
            "hook",
            &method,
            "catenary",
            &self.router.client_name,
            Some(entry_id),
            None,
            &serde_json::from_str::<Value>(&response).unwrap_or_default(),
        );

        writer.write_all(response.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.shutdown().await?;

        Ok(())
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

    // ── Envelope serialization tests ──────────────────────────────────

    #[test]
    fn envelope_result_only() {
        let env = HookResponseEnvelope {
            result: Some(HookResult::Content("[clean]".into())),
            system_message: None,
        };
        let json = serde_json::to_string(&env).expect("serialize");
        let parsed: HookResponseEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.result, Some(HookResult::Content("[clean]".into())));
        assert!(parsed.system_message.is_none());
        // system_message should be absent from JSON (skip_serializing_if)
        let raw: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert!(raw.get("system_message").is_none());
    }

    #[test]
    fn envelope_system_message_only() {
        let env = HookResponseEnvelope {
            result: None,
            system_message: Some("[warn] server offline".into()),
        };
        let json = serde_json::to_string(&env).expect("serialize");
        let parsed: HookResponseEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert!(parsed.result.is_none());
        assert_eq!(
            parsed.system_message.as_deref(),
            Some("[warn] server offline")
        );
        let raw: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert!(raw.get("result").is_none());
    }

    #[test]
    fn envelope_both_fields() {
        let env = HookResponseEnvelope {
            result: Some(HookResult::Cleared(2)),
            system_message: Some("─── background ───\n[warn] offline".into()),
        };
        let json = serde_json::to_string(&env).expect("serialize");
        let parsed: HookResponseEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.result, Some(HookResult::Cleared(2)));
        assert!(
            parsed
                .system_message
                .as_ref()
                .is_some_and(|m| m.contains("offline"))
        );
    }

    #[test]
    fn envelope_empty_is_default() {
        let env = HookResponseEnvelope::default();
        assert!(env.result.is_none());
        assert!(env.system_message.is_none());
        let json = serde_json::to_string(&env).expect("serialize");
        assert_eq!(json, "{}");
    }

    // ── Per-host response shape tests ──────────────────────────────────

    #[test]
    fn claude_code_response_shape() {
        // Stop hook allow with background drain.
        let env = HookResponseEnvelope {
            result: None,
            system_message: Some("─── background ───\n[warn] ra offline".into()),
        };
        let json = serde_json::to_string(&env).expect("serialize");
        // Claude Code reads systemMessage from the hook response.
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(
            parsed["system_message"].as_str(),
            Some("─── background ───\n[warn] ra offline"),
        );
    }

    #[test]
    fn gemini_cli_response_shape() {
        // AfterAgent hook allow with background drain.
        let env = HookResponseEnvelope {
            result: None,
            system_message: Some("─── background ───\n[err] pylsp crashed".into()),
        };
        let json = serde_json::to_string(&env).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(
            parsed["system_message"].as_str(),
            Some("─── background ───\n[err] pylsp crashed"),
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
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! IPC server for host CLI hook integration.
//!
//! `HookServer` is a protocol boundary: it parses IPC requests, dispatches
//! to the appropriate tool server (`DiagnosticsServer` or `SyncRootsServer`),
//! logs protocol messages, and formats responses. The transformation logic
//! lives in the tool server implementations under `bridge/`.
//!
//! Transport: Unix domain sockets on Unix, named pipes on Windows.

use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixListener;
use tracing::{debug, info, warn};

use crate::bridge::diagnostics_server::DiagnosticsServer;
use crate::bridge::sync_roots_server::SyncRootsServer;
use crate::session::{EventBroadcaster, EventKind, MessageLog};

/// Request from `catenary hook PostToolUse` (file change) or `catenary hook PreToolUse` (root sync).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum NotifyRequest {
    /// A file-change notification.
    File {
        /// Absolute path to the changed file.
        file: String,
        /// Name of the host CLI tool that triggered the hook (e.g., "Write", "Edit").
        /// Included in the logged payload for monitor visibility.
        #[serde(default)]
        #[allow(
            dead_code,
            reason = "metadata field — logged in payload, not read in code"
        )]
        tool: Option<String>,
    },
    /// A request to synchronize workspace roots (full replacement).
    SyncRoots {
        /// Complete set of workspace roots — server diffs against current state.
        sync_roots: Vec<String>,
    },
    /// A request to add new workspace roots (incremental).
    AddRoots {
        /// Absolute paths of directories to add as roots.
        add_roots: Vec<String>,
    },
}

/// IPC response from the hook server to the CLI.
///
/// Separates diagnostic content (for the model via `additionalContext`) from
/// internal errors (for the user via `systemMessage`). The CLI deserializes
/// this to decide where to route the output.
#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotifyResult {
    /// Diagnostic content for the model (may be `[clean]`, `[no language server]`,
    /// `[diagnostics unavailable]`, or formatted diagnostic lines).
    Content(String),
    /// Internal error for the user (path resolution, LSP client failures, etc.).
    Error(String),
}

/// Listens on an IPC endpoint (Unix socket or named pipe) for hook requests
/// from the host CLI and returns LSP diagnostics or root sync results.
pub struct HookServer {
    diagnostics: Arc<DiagnosticsServer>,
    sync_roots: SyncRootsServer,
    broadcaster: EventBroadcaster,
    message_log: Arc<MessageLog>,
    client_name: String,
}

impl HookServer {
    /// Creates a new `HookServer`.
    #[must_use]
    pub const fn new(
        diagnostics: Arc<DiagnosticsServer>,
        sync_roots: SyncRootsServer,
        broadcaster: EventBroadcaster,
        message_log: Arc<MessageLog>,
        client_name: String,
    ) -> Self {
        Self {
            diagnostics,
            sync_roots,
            broadcaster,
            message_log,
            client_name,
        }
    }

    /// Starts listening on the given IPC endpoint.
    ///
    /// Spawns a background task that accepts connections and processes
    /// file-change notifications. Returns a `JoinHandle` for the listener task.
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
    /// file-change notifications. Returns a `JoinHandle` for the listener task.
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

    /// Handles a single connection: reads a JSON request, dispatches to the
    /// appropriate handler, and writes back the response.
    async fn handle_connection<S: AsyncRead + AsyncWrite + Unpin>(&self, stream: S) -> Result<()> {
        let (reader, mut writer) = tokio::io::split(stream);
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        buf_reader.read_line(&mut line).await?;

        let request: NotifyRequest =
            serde_json::from_str(line.trim()).map_err(|e| anyhow!("Invalid request: {e}"))?;

        let method = match &request {
            NotifyRequest::File { .. } => "post-tool",
            NotifyRequest::SyncRoots { .. } | NotifyRequest::AddRoots { .. } => "pre-tool",
        };

        // Log incoming hook request
        let entry_id = self.message_log.log(
            "hook",
            method,
            "catenary",
            &self.client_name,
            None,
            None,
            &serde_json::from_str::<Value>(line.trim()).unwrap_or_default(),
        );

        let response = match request {
            NotifyRequest::File { file, .. } => {
                debug!("Hook: processing file {file}");
                self.process_file(&file, entry_id).await
            }
            NotifyRequest::SyncRoots { sync_roots } => {
                debug!("Hook: syncing {} root(s)", sync_roots.len());
                self.sync_roots.sync_roots(&sync_roots).await
            }
            NotifyRequest::AddRoots { add_roots } => {
                debug!("Hook: adding {} root(s)", add_roots.len());
                self.sync_roots.add_roots(&add_roots).await
            }
        };

        // Log outgoing hook response
        self.message_log.log(
            "hook",
            method,
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

    /// Processes a file change notification and returns a [`NotifyResult`] as JSON.
    async fn process_file(&self, file_path: &str, entry_id: i64) -> String {
        let result = match self.diagnostics.process_file(file_path, entry_id).await {
            Ok(diag_result) => {
                // Broadcast diagnostics event for monitor visibility
                self.broadcaster.send(EventKind::Diagnostics {
                    file: file_path.to_string(),
                    count: diag_result.count,
                    preview: diag_result.content.clone(),
                });
                NotifyResult::Content(diag_result.content)
            }
            Err(e) => {
                warn!("Notify error for {file_path}: {e}");
                NotifyResult::Error(e.to_string())
            }
        };
        // Safe: NotifyResult serialization cannot fail (no non-string map keys, no floats).
        serde_json::to_string(&result).unwrap_or_default()
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
    fn notify_result_content_round_trip() {
        let original = NotifyResult::Content("[clean]".into());
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: NotifyResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);

        let raw: serde_json::Value = serde_json::from_str(&json).expect("parse as value");
        assert_eq!(raw["content"], "[clean]");
        assert!(raw.get("error").is_none());
    }

    #[test]
    fn notify_result_error_round_trip() {
        let original = NotifyResult::Error("path resolution failed".into());
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: NotifyResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);

        let raw: serde_json::Value = serde_json::from_str(&json).expect("parse as value");
        assert_eq!(raw["error"], "path resolution failed");
        assert!(raw.get("content").is_none());
    }

    #[test]
    fn test_hook_request_tool_field() {
        // With tool field
        let json = r#"{"file": "/tmp/test.rs", "tool": "Write"}"#;
        let req: NotifyRequest = serde_json::from_str(json).expect("deserialize with tool");
        let NotifyRequest::File { file, tool } = req else {
            unreachable!("expected File variant");
        };
        assert_eq!(file, "/tmp/test.rs");
        assert_eq!(tool.as_deref(), Some("Write"));

        // Without tool field (backward compatibility)
        let json = r#"{"file": "/tmp/test.rs"}"#;
        let req: NotifyRequest = serde_json::from_str(json).expect("deserialize without tool");
        let NotifyRequest::File { tool, .. } = req else {
            unreachable!("expected File variant");
        };
        assert!(tool.is_none());
    }

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

        // Simulate what handle_connection does for a File request
        let method = "post-tool";
        let request_payload = serde_json::json!({"file": "/tmp/test.rs", "tool": "Write"});
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
        assert_eq!(r_method, "post-tool");

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
    fn test_hook_log_sync_roots() {
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

        let method = "pre-tool";
        let request_payload = serde_json::json!({"sync_roots": ["/tmp/root1"]});
        let entry_id = log.log(
            "hook",
            method,
            "catenary",
            "host",
            None,
            None,
            &request_payload,
        );

        let response_payload = serde_json::json!("Added roots: /tmp/root1");
        log.log(
            "hook",
            method,
            "catenary",
            "host",
            Some(entry_id),
            None,
            &response_payload,
        );

        // Verify method is pre-tool
        let r_method: String = conn
            .lock()
            .expect("lock")
            .query_row(
                "SELECT method FROM messages WHERE id = ?1",
                [entry_id],
                |row| row.get(0),
            )
            .expect("query");
        assert_eq!(r_method, "pre-tool");
    }

    /// Open an isolated test database in a tempdir.
    fn test_db() -> (tempfile::TempDir, std::path::PathBuf, rusqlite::Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catenary").join("catenary.db");
        let conn = crate::db::open_and_migrate_at(&path).expect("open test DB");
        (dir, path, conn)
    }
}

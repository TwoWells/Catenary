// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Application dispatch for hook requests.
//!
//! `HookRouter` owns all hook method handlers and application logic
//! (editing state enforcement, diagnostics dispatch, root refresh signaling).
//! Mirrors the [`super::handler::McpRouter`] pattern: protocol boundary
//! delegates to router, router delegates to [`super::toolbox::Toolbox`].

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::debug;

use super::toolbox::Toolbox;
use crate::hook::response::SystemMessageBuilder;
use crate::hook::{HookRequest, HookResult};

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

/// Result of hook dispatch: the handler's result plus an optional
/// `systemMessage` from the notification queue drain.
pub struct DispatchResult {
    /// Handler result (`None` = allow / no actionable data).
    pub result: Option<HookResult>,
    /// Composed `systemMessage` content (direct + background drain).
    pub system_message: Option<String>,
}

// ── HookRouter ──────────────────────────────────────────────────────────

/// Application dispatch for hook requests.
///
/// Routes parsed [`HookRequest`] values to the appropriate handler and
/// returns an optional [`HookResult`]. Holds all shared application state
/// needed by hook handlers: editing state (via [`super::editing_manager::EditingManager`]
/// on [`Toolbox`]), and root refresh signaling.
pub struct HookRouter {
    toolbox: Arc<Toolbox>,
    refresh_roots: Arc<AtomicBool>,
    conn: Arc<std::sync::Mutex<rusqlite::Connection>>,
    session_id: String,
    /// Host CLI client name (e.g., `"host"`, `"claude-code"`).
    pub(crate) client_name: String,
}

impl HookRouter {
    /// Creates a new `HookRouter`.
    #[must_use]
    pub const fn new(
        toolbox: Arc<Toolbox>,
        refresh_roots: Arc<AtomicBool>,
        conn: Arc<std::sync::Mutex<rusqlite::Connection>>,
        session_id: String,
        client_name: String,
    ) -> Self {
        Self {
            toolbox,
            refresh_roots,
            conn,
            session_id,
            client_name,
        }
    }

    /// Dispatches a parsed hook request to the appropriate handler.
    ///
    /// Returns a [`DispatchResult`] with the handler's result and an optional
    /// `systemMessage` from the notification queue drain. The queue is drained
    /// only at stationary points (`SessionStart`, `Stop`/`AfterAgent` when allowing).
    pub(crate) fn dispatch(&self, request: HookRequest, _entry_id: i64) -> DispatchResult {
        match request {
            HookRequest::PreAgentRootsSync {} => {
                debug!("Hook: refresh roots requested");
                self.refresh_roots.store(true, Ordering::Release);
                DispatchResult {
                    result: None,
                    system_message: None,
                }
            }
            HookRequest::PreToolEnforceEditing {
                tool_name,
                file_path: _,
                agent_id,
                session_id,
            } => {
                self.store_client_session_id(session_id.as_deref());
                DispatchResult {
                    result: self.handle_enforce_editing(&tool_name, &agent_id),
                    system_message: None,
                }
            }
            HookRequest::PostToolDiagnostics {
                file,
                tool,
                agent_id,
                session_id,
            } => {
                self.store_client_session_id(session_id.as_deref());
                debug!("Hook: processing file {file}");
                DispatchResult {
                    result: self.handle_file_accumulation(&file, &agent_id, tool.as_deref()),
                    system_message: None,
                }
            }
            HookRequest::PostAgentRequireRelease {
                agent_id,
                stop_hook_active,
            } => {
                let result = self.handle_require_release(&agent_id, stop_hook_active);
                // Drain at stationary point: only when allowing the stop.
                let system_message = if matches!(result, Some(HookResult::Block(_))) {
                    None
                } else {
                    self.drain_notifications()
                };
                DispatchResult {
                    result,
                    system_message,
                }
            }
            HookRequest::SessionStartClearEditing { session_id } => {
                self.store_client_session_id(session_id.as_deref());
                let result = self.handle_clear_editing();
                // Drain at stationary point: session start.
                let system_message = self.drain_notifications();
                DispatchResult {
                    result,
                    system_message,
                }
            }
        }
    }

    /// Drain the notification queue into a `systemMessage` string.
    ///
    /// Returns `None` if the queue is empty and no sink panics occurred.
    fn drain_notifications(&self) -> Option<String> {
        let mut builder = SystemMessageBuilder::new();
        builder.drain_background(&self.toolbox.notifications, &self.toolbox.logging);
        builder.finish()
    }

    // ── Hook handlers ───────────────────────────────────────────────────

    /// Editing state enforcement: deny or allow a tool call.
    ///
    /// If the agent is in editing mode, only Edit/Read/Write and Catenary
    /// editing tools are allowed. If the agent is not in editing mode,
    /// Edit/Write requires `start_editing` first.
    ///
    /// When the tool is `start_editing`, enters editing mode as a side effect
    /// (the MCP tool is a trigger — the hook owns the state transition
    /// because it has the real `agent_id` from the host CLI).
    fn handle_enforce_editing(&self, tool_name: &str, agent_id: &str) -> Option<HookResult> {
        // start_editing: enter editing mode and allow unconditionally.
        if tool_name.contains("start_editing") {
            let _ = self.toolbox.editing.start_editing(agent_id);
            return None;
        }

        let agent_editing = self.toolbox.editing.is_editing(agent_id);

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

    /// Accumulates edited file paths during editing mode.
    ///
    /// When the agent is in editing mode and the tool is an edit tool,
    /// accumulates the file path for later batch diagnostics in
    /// `done_editing`. Always returns `None` — diagnostics are produced
    /// only by the MCP `done_editing` tool result.
    fn handle_file_accumulation(
        &self,
        file_path: &str,
        agent_id: &str,
        tool_name: Option<&str>,
    ) -> Option<HookResult> {
        if self.toolbox.editing.is_editing(agent_id) && tool_name.is_some_and(is_edit_tool) {
            self.toolbox
                .editing
                .add_file(agent_id, PathBuf::from(file_path));
        }
        None
    }

    /// Force `done_editing` before the agent stops.
    ///
    /// If `stop_hook_active` is true (retry after the agent failed to call
    /// `done_editing`), force-clears the stale editing state and allows.
    /// Otherwise blocks if the agent is in editing mode.
    fn handle_require_release(&self, agent_id: &str, stop_hook_active: bool) -> Option<HookResult> {
        if stop_hook_active {
            // Agent was told to call done_editing but didn't. Clear stale
            // state rather than leaving it for SessionStart/GC cleanup.
            self.toolbox.editing.done_editing(agent_id);
            return None;
        }

        if self.toolbox.editing.is_editing(agent_id) {
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
        let count = self.toolbox.editing.clear_all();

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

    use crate::config::Config;
    use crate::session::MessageLog;

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
        let router = test_router();
        // No editing state — Edit should be denied
        let result = router.handle_enforce_editing("Edit", "");
        let Some(HookResult::Deny(reason)) = result else {
            unreachable!("expected Deny, got {result:?}");
        };
        assert!(reason.contains("start_editing"));
    }

    #[test]
    fn test_hook_enforce_editing_allow() {
        let router = test_router();
        // Enter editing mode through the hook handler
        let result = router.handle_enforce_editing("start_editing", "");
        assert!(result.is_none(), "start_editing should allow");

        // Edit tool — should allow during editing mode
        let result = router.handle_enforce_editing("Edit", "");
        assert!(result.is_none(), "expected allow, got {result:?}");

        // Read tool — always allowed during editing
        let result = router.handle_enforce_editing("Read", "");
        assert!(result.is_none(), "expected allow for Read, got {result:?}");

        // Non-edit, non-read tool while editing — should deny
        let result = router.handle_enforce_editing("Bash", "");
        let Some(HookResult::Deny(reason)) = result else {
            unreachable!("expected Deny for Bash, got {result:?}");
        };
        assert!(reason.contains("done_editing"));
    }

    #[test]
    fn test_hook_enforce_editing_start_via_mcp_name() {
        let router = test_router();
        // MCP qualified name should also enter editing mode
        let result = router.handle_enforce_editing("mcp__catenary__start_editing", "");
        assert!(result.is_none(), "MCP start_editing should allow");
        assert!(
            router.toolbox.editing.is_editing(""),
            "should be in editing mode"
        );
    }

    #[test]
    fn test_hook_file_accumulation() {
        let router = test_router();
        router.handle_enforce_editing("start_editing", "");

        // Edit tool accumulates file
        let result = router.handle_file_accumulation("/src/main.rs", "", Some("Edit"));
        assert!(result.is_none());

        // Read tool does not accumulate
        let result = router.handle_file_accumulation("/src/lib.rs", "", Some("Read"));
        assert!(result.is_none());

        let files = router.toolbox.editing.drain_files("");
        assert_eq!(files, vec![std::path::PathBuf::from("/src/main.rs")]);
    }

    #[test]
    fn test_hook_require_release_block() {
        let router = test_router();
        // Enter editing mode through the hook handler
        router.handle_enforce_editing("start_editing", "");

        let result = router.handle_require_release("", false);
        let Some(HookResult::Block(reason)) = result else {
            unreachable!("expected Block, got {result:?}");
        };
        assert!(reason.contains("done_editing"));
    }

    #[test]
    fn test_hook_require_release_allow() {
        let router = test_router();
        // No editing state — should allow
        let result = router.handle_require_release("", false);
        assert!(result.is_none(), "expected allow, got {result:?}");
    }

    #[test]
    fn test_hook_require_release_retry() {
        let router = test_router();
        // Enter editing mode through the hook handler
        router.handle_enforce_editing("start_editing", "");

        // stop_hook_active = true → always allow regardless of state
        let result = router.handle_require_release("", true);
        assert!(result.is_none(), "expected allow on retry, got {result:?}");

        // State should be cleared
        assert!(
            !router.toolbox.editing.is_editing(""),
            "editing state should be cleared after retry"
        );
    }

    #[test]
    fn test_hook_clear_editing() {
        let router = test_router();
        // Enter editing mode for two agents through the hook handler
        router.handle_enforce_editing("start_editing", "");
        router.handle_enforce_editing("start_editing", "agent-b");

        let result = router.handle_clear_editing();
        assert_eq!(result, Some(HookResult::Cleared(2)));

        // Second call should return None (nothing to clear)
        let result = router.handle_clear_editing();
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

    /// Create a `HookRouter` with a test database for handler unit tests.
    ///
    /// Uses noop `MessageLog` and minimal dependencies. Editing state is
    /// managed in-memory via [`super::super::editing_manager::EditingManager`]
    /// on the `Toolbox`.
    fn test_router() -> TestHookRouter {
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
        let config: Config = serde_json::from_str("{}").expect("empty config");
        let logging = crate::logging::LoggingServer::new();

        // Toolbox requires a tokio runtime handle for async dispatch.
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let handle = runtime.handle().clone();

        let toolbox = Arc::new(Toolbox::new(
            config,
            vec![],
            message_log,
            logging,
            conn.clone(),
            "test-session".to_string(),
            handle,
        ));
        let refresh_roots = Arc::new(AtomicBool::new(false));

        let router = HookRouter::new(
            toolbox,
            refresh_roots,
            conn,
            "test-session".to_string(),
            "test".to_string(),
        );

        TestHookRouter {
            _dir: dir,
            _runtime: runtime,
            router,
        }
    }

    /// Wrapper that keeps the tempdir and runtime alive for the lifetime of the router.
    struct TestHookRouter {
        _dir: tempfile::TempDir,
        _runtime: tokio::runtime::Runtime,
        router: HookRouter,
    }

    impl std::ops::Deref for TestHookRouter {
        type Target = HookRouter;
        fn deref(&self) -> &Self::Target {
            &self.router
        }
    }

    // ── Dispatch-level drain tests ─────────────────────────────────────

    #[test]
    fn dispatch_session_start_drains_notifications() {
        let router = test_router();
        // Populate the notification queue.
        let event = crate::logging::LogEvent {
            severity: crate::logging::Severity::Warn,
            target: "test",
            message: "server offline".to_string(),
            kind: None,
            method: None,
            server: Some("ra".to_string()),
            client: None,
            request_id: None,
            parent_id: None,
            source: None,
            language: None,
            payload: None,
            fields: serde_json::Map::new(),
        };
        crate::logging::Sink::handle(router.toolbox.notifications.as_ref(), &event);
        assert_eq!(router.toolbox.notifications.len(), 1);

        let result = router.dispatch(
            crate::hook::HookRequest::SessionStartClearEditing { session_id: None },
            0,
        );
        assert!(
            result.system_message.is_some(),
            "session start should drain notifications"
        );
        assert!(router.toolbox.notifications.is_empty());
    }

    #[test]
    fn dispatch_stop_allow_drains_notifications() {
        let router = test_router();
        let event = crate::logging::LogEvent {
            severity: crate::logging::Severity::Warn,
            target: "test",
            message: "server offline".to_string(),
            kind: None,
            method: None,
            server: Some("ra".to_string()),
            client: None,
            request_id: None,
            parent_id: None,
            source: None,
            language: None,
            payload: None,
            fields: serde_json::Map::new(),
        };
        crate::logging::Sink::handle(router.toolbox.notifications.as_ref(), &event);

        // Not editing → allow → should drain.
        let result = router.dispatch(
            crate::hook::HookRequest::PostAgentRequireRelease {
                agent_id: String::new(),
                stop_hook_active: false,
            },
            0,
        );
        assert!(result.result.is_none(), "should allow");
        assert!(
            result.system_message.is_some(),
            "allow should drain notifications"
        );
        assert!(router.toolbox.notifications.is_empty());
    }

    #[test]
    fn dispatch_stop_block_preserves_notifications() {
        let router = test_router();
        // Enter editing mode so stop blocks.
        router.handle_enforce_editing("start_editing", "");

        let event = crate::logging::LogEvent {
            severity: crate::logging::Severity::Warn,
            target: "test",
            message: "server offline".to_string(),
            kind: None,
            method: None,
            server: Some("ra".to_string()),
            client: None,
            request_id: None,
            parent_id: None,
            source: None,
            language: None,
            payload: None,
            fields: serde_json::Map::new(),
        };
        crate::logging::Sink::handle(router.toolbox.notifications.as_ref(), &event);

        let result = router.dispatch(
            crate::hook::HookRequest::PostAgentRequireRelease {
                agent_id: String::new(),
                stop_hook_active: false,
            },
            0,
        );
        assert!(
            matches!(result.result, Some(HookResult::Block(_))),
            "should block"
        );
        assert!(result.system_message.is_none(), "block should not drain");
        assert_eq!(
            router.toolbox.notifications.len(),
            1,
            "queue should be preserved"
        );
    }

    #[test]
    fn dispatch_pre_tool_does_not_drain() {
        let router = test_router();
        let event = crate::logging::LogEvent {
            severity: crate::logging::Severity::Warn,
            target: "test",
            message: "server offline".to_string(),
            kind: None,
            method: None,
            server: Some("ra".to_string()),
            client: None,
            request_id: None,
            parent_id: None,
            source: None,
            language: None,
            payload: None,
            fields: serde_json::Map::new(),
        };
        crate::logging::Sink::handle(router.toolbox.notifications.as_ref(), &event);

        let result = router.dispatch(
            crate::hook::HookRequest::PreToolEnforceEditing {
                tool_name: "Read".to_string(),
                file_path: None,
                agent_id: String::new(),
                session_id: None,
            },
            0,
        );
        assert!(result.system_message.is_none(), "pre-tool should not drain");
        assert_eq!(router.toolbox.notifications.len(), 1);
    }

    #[test]
    fn dispatch_post_tool_does_not_drain() {
        let router = test_router();
        let event = crate::logging::LogEvent {
            severity: crate::logging::Severity::Warn,
            target: "test",
            message: "server offline".to_string(),
            kind: None,
            method: None,
            server: Some("ra".to_string()),
            client: None,
            request_id: None,
            parent_id: None,
            source: None,
            language: None,
            payload: None,
            fields: serde_json::Map::new(),
        };
        crate::logging::Sink::handle(router.toolbox.notifications.as_ref(), &event);

        let result = router.dispatch(
            crate::hook::HookRequest::PostToolDiagnostics {
                file: "/tmp/test.rs".to_string(),
                tool: Some("Edit".to_string()),
                agent_id: String::new(),
                session_id: None,
            },
            0,
        );
        assert!(
            result.system_message.is_none(),
            "post-tool should not drain"
        );
        assert_eq!(router.toolbox.notifications.len(), 1);
    }
}

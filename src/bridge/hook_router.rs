// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Application dispatch for hook requests.
//!
//! `HookRouter` owns all hook method handlers and application logic
//! (editing state enforcement, diagnostics dispatch, root refresh signaling).
//! Mirrors the [`super::handler::McpRouter`] pattern: protocol boundary
//! delegates to router, router delegates to [`super::toolbox::Toolbox`].

use rusqlite::Connection;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

use super::toolbox::Toolbox;
use crate::db;
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

// ── HookRouter ──────────────────────────────────────────────────────────

/// Application dispatch for hook requests.
///
/// Routes parsed [`HookRequest`] values to the appropriate handler and
/// returns an optional [`HookResult`]. Holds all shared application state
/// needed by hook handlers: editing state (via database), diagnostics
/// (via [`Toolbox`]), and root refresh signaling.
pub struct HookRouter {
    toolbox: Arc<Toolbox>,
    refresh_roots: Arc<AtomicBool>,
    conn: Arc<Mutex<Connection>>,
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
        conn: Arc<Mutex<Connection>>,
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
    /// Returns `None` for "allow" (empty response — CLI outputs nothing).
    /// Returns `Some(HookResult)` with actionable data for the CLI.
    pub(crate) async fn dispatch(&self, request: HookRequest, entry_id: i64) -> Option<HookResult> {
        match request {
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
            HookRequest::PostToolDoneEditing {
                agent_id,
                session_id,
            } => {
                self.store_client_session_id(session_id.as_deref());
                self.handle_done_editing(&agent_id, entry_id).await
            }
            HookRequest::PostAgentRequireRelease {
                agent_id,
                stop_hook_active,
            } => self.handle_require_release(&agent_id, stop_hook_active),
            HookRequest::SessionStartClearEditing { session_id } => {
                self.store_client_session_id(session_id.as_deref());
                self.handle_clear_editing()
            }
        }
    }

    // ── Hook handlers ───────────────────────────────────────────────────

    /// Editing state enforcement: deny or allow a tool call.
    ///
    /// If the agent is in editing mode, only Edit/Read/Write and Catenary
    /// editing tools are allowed. If the agent is not in editing mode,
    /// Edit/Write requires `start_editing` first.
    ///
    /// When the tool is `start_editing`, enters editing mode as a side effect
    /// (the MCP tool is a no-op trigger — the hook owns the state transition
    /// because it has the real `agent_id` from the host CLI).
    fn handle_enforce_editing(&self, tool_name: &str, agent_id: &str) -> Option<HookResult> {
        // start_editing: enter editing mode and allow unconditionally.
        if tool_name.contains("start_editing") {
            if let Ok(c) = self.conn.lock() {
                let _ = db::start_editing(&c, &self.session_id, agent_id);
            }
            return None;
        }

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

    /// Batch diagnostics on `done_editing`: exit editing mode, drain
    /// accumulated files, and run diagnostics on all of them.
    ///
    /// The MCP `done_editing` tool is a no-op trigger — the hook owns the
    /// state transition because it has the real `agent_id` from the host CLI.
    async fn handle_done_editing(&self, agent_id: &str, entry_id: i64) -> Option<HookResult> {
        let files = {
            let conn = self.conn.lock().ok()?;
            let files = db::drain_editing_files(&conn, &self.session_id, agent_id).ok()?;
            let _ = db::done_editing(&conn, &self.session_id, agent_id);
            drop(conn);
            files
        };

        let file_refs: Vec<&str> = files.iter().map(String::as_str).collect();
        let output = self
            .toolbox
            .diagnostics
            .process_files(&file_refs, entry_id)
            .await;

        if output.is_empty() {
            Some(HookResult::Content("done editing [clean]".into()))
        } else {
            Some(HookResult::Content(format!("done editing\n{output}")))
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
    /// If `stop_hook_active` is true (retry after the agent failed to call
    /// `done_editing`), force-clears the stale editing state and allows.
    /// Otherwise blocks if the agent is in editing mode.
    fn handle_require_release(&self, agent_id: &str, stop_hook_active: bool) -> Option<HookResult> {
        if stop_hook_active {
            // Agent was told to call done_editing but didn't. Clear stale
            // state rather than leaving it for SessionStart/GC cleanup.
            if let Ok(c) = self.conn.lock() {
                let _ = db::done_editing(&c, &self.session_id, agent_id);
            }
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
        // Enter editing mode
        router
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
    fn test_hook_require_release_block() {
        let router = test_router();
        // Enter editing mode
        router
            .conn
            .lock()
            .expect("lock")
            .execute(
                "INSERT INTO editing_state (session_id, agent_id, started_at) \
                 VALUES ('test-session', '', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert editing state");

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
        // Enter editing mode
        router
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
        let result = router.handle_require_release("", true);
        assert!(result.is_none(), "expected allow on retry, got {result:?}");
    }

    #[test]
    fn test_hook_clear_editing() {
        let router = test_router();
        // Enter editing mode for two agents
        {
            let c = router.conn.lock().expect("lock");
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
    /// Uses noop `MessageLog` and minimal dependencies — only `conn` and
    /// `session_id` are exercised by editing state handlers.
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

        // Toolbox requires a tokio runtime handle for async dispatch.
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let handle = runtime.handle().clone();

        let toolbox = Arc::new(Toolbox::new(
            config,
            vec![],
            message_log,
            String::new(),
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
}

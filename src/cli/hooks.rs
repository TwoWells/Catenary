// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Hook handlers for host CLI integration.
//!
//! Function names mirror the hook lifecycle:
//! - `run_pre_agent` — root sync (`UserPromptSubmit` / `BeforeAgent`)
//! - `run_pre_tool` — editing state enforcement (`PreToolUse` / `BeforeTool`)
//! - `run_post_tool` — diagnostics (`PostToolUse` / `AfterTool`)
//! - `run_post_agent` — force `done_editing` (`Stop` / `AfterAgent`)

#![allow(clippy::print_stdout, reason = "CLI tool needs to output to stdout")]
#![allow(clippy::print_stderr, reason = "CLI tool needs to output to stderr")]

use std::path::PathBuf;
use std::time::Duration;

use crate::cli::HostFormat;
use crate::{db, session};

/// Returns the IPC endpoint path for a session.
///
/// On Unix this is the Unix socket path in the session directory.
/// On Windows this is a named pipe in the kernel namespace.
fn notify_endpoint(session_id: &str) -> PathBuf {
    #[cfg(unix)]
    {
        session::sessions_dir().join(session_id).join("notify.sock")
    }
    #[cfg(windows)]
    {
        PathBuf::from(format!(r"\\.\pipe\catenary-{session_id}"))
    }
}

/// Connects to a notify IPC endpoint and returns a stream for I/O.
///
/// Returns `None` silently on failure (hooks must not break Claude Code's flow).
#[cfg(unix)]
fn notify_connect(endpoint: &std::path::Path) -> Option<std::os::unix::net::UnixStream> {
    if !endpoint.exists() {
        return None;
    }
    let stream = std::os::unix::net::UnixStream::connect(endpoint).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    Some(stream)
}

/// Connects to a notify IPC endpoint and returns a stream for I/O.
///
/// Returns `None` silently on failure (hooks must not break Claude Code's flow).
#[cfg(windows)]
fn notify_connect(endpoint: &std::path::Path) -> Option<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    // SECURITY_IDENTIFICATION (0x0001_0000) prevents impersonation attacks
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .security_qos_flags(0x0001_0000)
        .open(endpoint)
        .ok()
}

/// Sends a JSON request over an IPC stream and reads response lines.
fn ipc_exchange(
    mut stream: impl std::io::Read + std::io::Write,
    request: &serde_json::Value,
) -> Vec<String> {
    use std::io::BufRead;

    if serde_json::to_writer(&mut stream, request).is_err() {
        return Vec::new();
    }
    if stream.write_all(b"\n").is_err() || stream.flush().is_err() {
        return Vec::new();
    }

    let reader = std::io::BufReader::new(stream);
    let mut lines = Vec::new();
    for line in reader.lines() {
        match line {
            Ok(text) if !text.is_empty() => lines.push(text),
            _ => break,
        }
    }
    lines
}

/// Returns `true` if the tool is an edit tool that requires `start_editing`.
fn is_edit_tool(tool_name: &str, format: HostFormat) -> bool {
    match format {
        HostFormat::Claude => matches!(tool_name, "Edit" | "Write" | "NotebookEdit"),
        HostFormat::Gemini => matches!(tool_name, "write_file" | "replace"),
    }
}

/// Returns `true` if the tool is a read tool (always allowed during editing).
fn is_read_tool(tool_name: &str, format: HostFormat) -> bool {
    match format {
        HostFormat::Claude => matches!(tool_name, "Read" | "NotebookRead"),
        HostFormat::Gemini => tool_name == "read_file",
    }
}

/// Returns `true` if the tool is always allowed during editing mode.
///
/// Catenary editing tools (`start_editing`, `done_editing`) must be allowed
/// so the agent can manage editing state. `ToolSearch` must be allowed
/// because both editing tools are deferred in Claude Code — blocking
/// ToolSearch while editing creates an unrecoverable state if the agent
/// loaded `start_editing` but not `done_editing` before entering editing mode.
fn is_allowed_during_editing(tool_name: &str) -> bool {
    tool_name.contains("start_editing")
        || tool_name.contains("done_editing")
        || tool_name == "ToolSearch"
}

/// Format a PreToolUse deny response for the host CLI.
fn format_deny(reason: &str, format: HostFormat) -> String {
    match format {
        HostFormat::Claude => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason
            }
        })
        .to_string(),
        HostFormat::Gemini => serde_json::json!({
            "decision": "deny",
            "reason": reason
        })
        .to_string(),
    }
}

/// Format a Stop/AfterAgent block response for the host CLI.
fn format_stop_block(reason: &str, format: HostFormat) -> String {
    match format {
        HostFormat::Claude => serde_json::json!({
            "decision": "block",
            "reason": reason
        })
        .to_string(),
        HostFormat::Gemini => serde_json::json!({
            "decision": "retry",
            "reason": reason
        })
        .to_string(),
    }
}

/// Find the Catenary session ID for a hook payload, using the working directory
/// to match against workspace roots. Returns `None` if no matching session.
fn find_session_id(
    hook_json: &serde_json::Value,
    conn: &rusqlite::Connection,
) -> Option<String> {
    let cwd = hook_json
        .get("cwd")
        .and_then(|v| v.as_str())
        .map_or_else(
            || std::env::current_dir().unwrap_or_default(),
            PathBuf::from,
        );
    let cwd_str = cwd.to_string_lossy();
    let sessions = session::list_sessions_with_conn(conn).unwrap_or_default();
    sessions
        .into_iter()
        .find(|(s, alive)| *alive && cwd_str.starts_with(&s.workspace))
        .map(|(s, _)| s.id)
}

/// Extract `agent_id` from hook payload. Defaults to empty string (main agent).
fn extract_agent_id(hook_json: &serde_json::Value) -> &str {
    hook_json
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

/// Clear all editing state for a session (`SessionStart` hook handler).
///
/// Called on session start, resume, `/clear`, and `/compact`. The agent's
/// context is gone, so stale editing state must be cleared. No diagnostics
/// are delivered.
pub fn run_session_start(format: HostFormat) {
    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        return;
    };
    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        return;
    };

    let Ok(conn) = db::open_and_migrate() else {
        return;
    };

    let Some(catenary_sid) = find_session_id(&hook_json, &conn) else {
        return;
    };

    let count = db::clear_session_editing(&conn, &catenary_sid).unwrap_or(0);
    if count > 0 {
        // Store host session ID while we have the connection.
        if let Some(client_sid) = hook_json.get("session_id").and_then(|v| v.as_str()) {
            let _ = conn.execute(
                "UPDATE sessions SET client_session_id = ?1 \
                 WHERE id = ?2 AND client_session_id IS NULL",
                rusqlite::params![client_sid, &catenary_sid],
            );
        }
        // Log for verbose mode only — not injected into model context.
        let msg = format!("Catenary: cleared {count} stale editing state entries");
        let output = match format {
            HostFormat::Claude => serde_json::json!({ "systemMessage": msg }),
            HostFormat::Gemini => serde_json::json!({ "systemMessage": msg }),
        };
        print!("{output}");
    }
}

/// Force `done_editing` before the agent finishes responding (`Stop` / `AfterAgent`
/// hook handler).
///
/// If the agent has files in editing state, blocks the stop with a message
/// directing the agent to call `done_editing`. If `stop_hook_active` is true
/// (Claude Code) indicating a retry, allows the stop — SessionStart cleanup
/// handles the stale state.
pub fn run_post_agent(format: HostFormat) {
    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        return;
    };
    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        return;
    };

    // Prevent infinite loops: if this is already a retry, let the agent stop.
    let stop_hook_active = hook_json
        .get("stop_hook_active")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if stop_hook_active {
        return;
    }

    let Ok(conn) = db::open_and_migrate() else {
        return;
    };

    let Some(catenary_sid) = find_session_id(&hook_json, &conn) else {
        return;
    };

    let agent_id = extract_agent_id(&hook_json);
    let files = db::editing_files_for_agent(&conn, &catenary_sid, agent_id).unwrap_or_default();

    if files.is_empty() {
        return;
    }

    let file_list = files.join(", ");
    let reason = format!(
        "call done_editing for {file_list} to get diagnostics before finishing"
    );
    print!("{}", format_stop_block(&reason, format));
}

/// Run diagnostics after reading or editing (`PostToolUse` / `AfterTool` hook handler).
///
/// Reads hook JSON from stdin, finds the session for the file's workspace,
/// connects to the notify socket, and returns diagnostics for the model's
/// context. Emits `systemMessage` JSON on infrastructure errors so the user
/// sees failures in their terminal.
pub fn run_post_tool(format: HostFormat) {
    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        print!(
            "{}",
            notify_error(
                "hook input unavailable — try restarting your session",
                format
            )
        );
        return;
    };

    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        print!(
            "{}",
            notify_error(
                "unexpected hook input — try restarting your session",
                format
            )
        );
        return;
    };

    let Some(file_path) = extract_file_path(&hook_json) else {
        print!(
            "{}",
            notify_error(
                "missing file path in hook input — diagnostics skipped",
                format
            )
        );
        return;
    };

    // Notify session for diagnostics
    let abs_path = PathBuf::from(&file_path);
    let Ok(conn) = crate::db::open_and_migrate() else {
        print!(
            "{}",
            notify_error(
                "state database unavailable — try running: catenary list",
                format
            )
        );
        return;
    };
    let sessions = session::list_sessions_with_conn(&conn).unwrap_or_default();
    let session = sessions
        .iter()
        .find(|(s, alive)| *alive && abs_path.to_string_lossy().starts_with(&s.workspace));

    let Some((session, _)) = session else {
        // File is outside all session workspaces, or no alive session — nothing to do.
        // No systemMessage: this isn't actionable for the user.
        return;
    };

    // Store the host CLI's session ID (first hook wins, subsequent calls are no-ops).
    if let Some(client_sid) = hook_json.get("session_id").and_then(|v| v.as_str()) {
        let _ = conn.execute(
            "UPDATE sessions SET client_session_id = ?1 \
             WHERE id = ?2 AND client_session_id IS NULL",
            rusqlite::params![client_sid, &session.id],
        );
    }

    // --- Editing state: suppress diagnostics for files being edited ---
    let agent_id = extract_agent_id(&hook_json);
    let self_editing =
        db::is_editing(&conn, &file_path, &session.id, agent_id).unwrap_or(false);
    if self_editing {
        // This agent is editing this file — suppress diagnostics entirely.
        return;
    }
    let other_editing =
        db::is_edited_by_others(&conn, &file_path, &session.id, agent_id).unwrap_or(false);

    let endpoint = notify_endpoint(&session.id);
    let Some(stream) = notify_connect(&endpoint) else {
        print!(
            "{}",
            notify_error(
                &format!(
                    "session {} is not responding — it may have crashed",
                    session.id
                ),
                format,
            )
        );
        return;
    };

    let tool_name = hook_json.get("tool_name").and_then(|v| v.as_str());
    let mut request = serde_json::json!({ "file": abs_path.to_string_lossy() });
    if let Some(tool) = tool_name {
        request["tool"] = serde_json::json!(tool);
    }
    let lines = ipc_exchange(stream, &request);

    // Deserialize NotifyResult from the first response line.
    // Graceful degradation: if parsing fails (version skew), treat raw lines as content.
    let result = lines
        .first()
        .and_then(|line| serde_json::from_str::<crate::hook::NotifyResult>(line).ok())
        .unwrap_or_else(|| crate::hook::NotifyResult::Content(lines.join("\n")));

    match result {
        crate::hook::NotifyResult::Content(content) => {
            let filename = std::path::Path::new(&file_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&file_path);
            let courtesy = if other_editing {
                "\n\t[diagnostics for this file are being deferred by another agent]"
            } else {
                ""
            };
            let full_content = format!("{filename}\n\t{content}{courtesy}");
            let output = format_diagnostics(&full_content, format, "PostToolUse");
            print!("{output}");
        }
        crate::hook::NotifyResult::Error(msg) => {
            print!("{}", notify_error(&msg, format));
        }
    }
}

/// Refresh workspace roots (`UserPromptSubmit` / `BeforeAgent` hook handler).
///
/// Sends a `refresh_roots` IPC request to the running Catenary session so
/// `/add-dir` workspace additions are picked up. Runs once per user prompt
/// rather than on every tool call.
///
/// Silently succeeds on any error to avoid breaking the host CLI's flow.
pub fn run_pre_agent(format: HostFormat) {
    let _ = format; // Reserved for future per-host output formatting.

    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        return;
    };

    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        return;
    };

    let Ok(conn) = db::open_and_migrate() else {
        return;
    };

    if let Some(catenary_sid) = find_session_id(&hook_json, &conn) {
        let endpoint = notify_endpoint(&catenary_sid);
        if let Some(stream) = notify_connect(&endpoint) {
            let request = serde_json::json!({ "refresh_roots": true });
            let _ = ipc_exchange(stream, &request);
        }
    }
}

/// Editing state enforcement (`PreToolUse` / `BeforeTool` hook handler).
///
/// Checks editing state before allowing tool execution. If the agent is
/// editing files, only Edit/Read on those files and Catenary editing tools
/// are allowed. If the agent is not editing, Edit requires `start_editing`
/// first.
///
/// Silently succeeds on any error to avoid breaking the host CLI's flow.
pub fn run_pre_tool(format: HostFormat) {
    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        return;
    };

    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        return;
    };

    let tool_name = hook_json
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let Ok(conn) = db::open_and_migrate() else {
        // Can't check editing state — allow the tool to proceed.
        return;
    };

    if let Some(catenary_sid) = find_session_id(&hook_json, &conn) {
        // Store host session ID.
        if let Some(client_sid) = hook_json.get("session_id").and_then(|v| v.as_str()) {
            let _ = conn.execute(
                "UPDATE sessions SET client_session_id = ?1 \
                 WHERE id = ?2 AND client_session_id IS NULL",
                rusqlite::params![client_sid, &catenary_sid],
            );
        }

        let agent_id = extract_agent_id(&hook_json);
        let editing_files =
            db::editing_files_for_agent(&conn, &catenary_sid, agent_id).unwrap_or_default();

        if !editing_files.is_empty() {
            // Agent is editing files — restrict to Edit/Read on those files,
            // plus Catenary editing tools. Deny everything else.
            if is_allowed_during_editing(tool_name) || is_read_tool(tool_name, format) {
                // Always allowed.
            } else if is_edit_tool(tool_name, format) {
                // Edit tool: check if the target file is being edited.
                if let Some(file_path) = extract_file_path(&hook_json) {
                    let is_managed = db::is_editing(&conn, &file_path, &catenary_sid, agent_id)
                        .unwrap_or(false);
                    if !is_managed {
                        let file_list = editing_files.join(", ");
                        let reason = format!(
                            "call start_editing for {file_path} before editing, \
                             or done_editing for {file_list} first"
                        );
                        print!("{}", format_deny(&reason, format));
                        return;
                    }
                }
                // File is being edited by this agent — allow.
            } else {
                // Non-Edit, non-Read tool while editing — deny.
                let file_list = editing_files.join(", ");
                let reason = format!(
                    "call done_editing for {file_list} to get diagnostics"
                );
                print!("{}", format_deny(&reason, format));
                return;
            }
        } else if is_edit_tool(tool_name, format) {
            // Agent is not editing any files + Edit tool — deny.
            let file_path = extract_file_path(&hook_json).unwrap_or_default();
            let reason = format!(
                "call start_editing for {file_path} before editing"
            );
            print!("{}", format_deny(&reason, format));
            return;
        }
    }
}

/// Extracts the file path from hook JSON's `tool_input`.
fn extract_file_path(hook_json: &serde_json::Value) -> Option<String> {
    let file_path = hook_json
        .get("tool_input")
        .and_then(|ti| ti.get("file_path").or_else(|| ti.get("file")))
        .and_then(|fp| fp.as_str())?;

    // Resolve to absolute path
    let abs_path = if std::path::Path::new(file_path).is_absolute() {
        PathBuf::from(file_path)
    } else {
        let cwd = hook_json.get("cwd").and_then(|v| v.as_str()).map_or_else(
            || std::env::current_dir().unwrap_or_default(),
            PathBuf::from,
        );
        cwd.join(file_path)
    };

    Some(abs_path.to_string_lossy().into_owned())
}

/// Format diagnostic content for the model via `additionalContext`.
///
/// Both formats wrap content in a `hookSpecificOutput` JSON envelope
/// so the host CLI can inject it into the model's context:
///
/// - Claude: includes `hookEventName` + `additionalContext` (required by
///   the Claude Code hook contract).
/// - Gemini: uses `additionalContext` only (no `hookEventName`).
fn format_diagnostics(content: &str, format: HostFormat, hook_event: &str) -> String {
    match format {
        HostFormat::Gemini => serde_json::json!({
            "hookSpecificOutput": {
                "additionalContext": content
            }
        })
        .to_string(),
        HostFormat::Claude => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": hook_event,
                "additionalContext": content
            }
        })
        .to_string(),
    }
}

/// GitHub issues URL for user-facing bug report suggestions.
const BUG_REPORT_URL: &str = "https://github.com/MarkWells-Dev/Catenary/issues";

/// Format an internal error for the user via `systemMessage`, with a bug
/// report link appended.
///
/// The error is shown to the user in their terminal but not injected into
/// the model's context — the model cannot act on internal Catenary failures.
fn notify_error(message: &str, format: HostFormat) -> String {
    let full =
        format!("Catenary: {message}. If this persists, please file a bug: {BUG_REPORT_URL}");
    format_error(&full, format)
}

/// Format an internal error for the user via `systemMessage`.
///
/// The error is shown to the user in their terminal but not injected into
/// the model's context — the model cannot act on internal Catenary failures.
fn format_error(message: &str, format: HostFormat) -> String {
    match format {
        HostFormat::Claude => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
            },
            "systemMessage": message
        })
        .to_string(),
        HostFormat::Gemini => serde_json::json!({
            "hookSpecificOutput": {},
            "systemMessage": message
        })
        .to_string(),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
#[allow(
    clippy::similar_names,
    reason = "content/context are distinct concepts in hook output tests"
)]
mod tests {
    use super::*;
    use anyhow::{Context, Result};

    #[test]
    fn test_format_diagnostics_claude() -> Result<()> {
        let content = "error[E0308]: mismatched types\n  --> src/main.rs:5:10";
        let output = format_diagnostics(content, HostFormat::Claude, "PostToolUse");
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("claude format should produce valid JSON")?;

        let hook_output = &parsed["hookSpecificOutput"];
        assert_eq!(hook_output["hookEventName"], "PostToolUse");
        let context = hook_output["additionalContext"]
            .as_str()
            .expect("additionalContext should be a string");
        assert!(context.contains("error[E0308]: mismatched types"));
        assert!(context.contains("  --> src/main.rs:5:10"));
        Ok(())
    }

    #[test]
    fn test_format_diagnostics_gemini() -> Result<()> {
        let content = "error[E0308]: mismatched types";
        let output = format_diagnostics(content, HostFormat::Gemini, "PostToolUse");
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("gemini format should produce valid JSON")?;

        let context = parsed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("additionalContext should be a string");
        assert_eq!(context, content);
        // Gemini format should NOT have hookEventName
        assert!(parsed["hookSpecificOutput"]["hookEventName"].is_null());
        Ok(())
    }

    #[test]
    fn test_format_diagnostics_gemini_multiline() -> Result<()> {
        let content = "warning: unused variable\n  --> lib.rs:3:9";
        let output = format_diagnostics(content, HostFormat::Gemini, "PostToolUse");
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;
        let context = parsed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("additionalContext should be a string");
        assert!(context.contains("warning: unused variable\n  --> lib.rs:3:9"));
        Ok(())
    }

    #[test]
    fn test_format_diagnostics_claude_propagates_hook_event() -> Result<()> {
        let content = "Added roots: /tmp/foo";
        let output = format_diagnostics(content, HostFormat::Claude, "PreToolUse");
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;

        assert_eq!(parsed["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        Ok(())
    }

    #[test]
    fn test_format_error_claude() -> Result<()> {
        let output = format_error("Catenary: database unavailable", HostFormat::Claude);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;

        assert_eq!(parsed["systemMessage"], "Catenary: database unavailable");
        assert_eq!(parsed["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert!(parsed["hookSpecificOutput"]["additionalContext"].is_null());
        Ok(())
    }

    #[test]
    fn test_format_error_gemini() -> Result<()> {
        let output = format_error("Catenary: database unavailable", HostFormat::Gemini);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).context("should produce valid JSON")?;

        assert_eq!(parsed["systemMessage"], "Catenary: database unavailable");
        assert!(parsed["hookSpecificOutput"]["hookEventName"].is_null());
        assert!(parsed["hookSpecificOutput"]["additionalContext"].is_null());
        Ok(())
    }
}

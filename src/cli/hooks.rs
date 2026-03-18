// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Hook handlers for host CLI integration (diagnostics and root sync).

#![allow(clippy::print_stdout, reason = "CLI tool needs to output to stdout")]
#![allow(clippy::print_stderr, reason = "CLI tool needs to output to stderr")]

use std::path::PathBuf;
use std::time::Duration;

use crate::cli::HostFormat;
use crate::session;

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

/// Run diagnostics notify after reading or editing (`PostToolUse` hook handler).
///
/// Reads hook JSON from stdin, finds the session for the file's workspace,
/// connects to the notify socket, and returns diagnostics for the model's
/// context. Emits `systemMessage` JSON on infrastructure errors so the user
/// sees failures in their terminal.
pub fn run_notify(format: HostFormat) {
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
            let full_content = format!("{filename}\n\t{content}");
            let output = format_diagnostics(&full_content, format, "PostToolUse");
            print!("{output}");
        }
        crate::hook::NotifyResult::Error(msg) => {
            print!("{}", notify_error(&msg, format));
        }
    }
}

/// Signal a running Catenary session to refresh workspace roots via MCP `roots/list`.
///
/// Reads hook JSON from stdin, finds the session for the working directory,
/// connects to the IPC endpoint, and sends a `refresh_roots` request. The
/// session's hook server sets an `AtomicBool` that the MCP server checks
/// before processing the next tool call, triggering a `roots/list` fetch.
///
/// Silently succeeds on any error to avoid breaking the host CLI's flow.
pub fn run_sync_roots(_format: HostFormat) {
    let Ok(stdin_data) = std::io::read_to_string(std::io::stdin()) else {
        return;
    };

    let Ok(hook_json) = serde_json::from_str::<serde_json::Value>(&stdin_data) else {
        return;
    };

    let cwd = hook_json.get("cwd").and_then(|v| v.as_str()).map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        PathBuf::from,
    );

    let Ok(db) = crate::db::open_and_migrate() else {
        return;
    };

    let sessions = session::list_sessions_with_conn(&db).unwrap_or_default();
    let cwd_str = cwd.to_string_lossy();
    let session = sessions
        .iter()
        .find(|(s, alive)| *alive && cwd_str.starts_with(&s.workspace));

    let Some((session, _)) = session else {
        return;
    };

    // Store the host CLI's session ID (first hook wins, subsequent calls are no-ops).
    if let Some(client_sid) = hook_json.get("session_id").and_then(|v| v.as_str()) {
        let _ = db.execute(
            "UPDATE sessions SET client_session_id = ?1 \
             WHERE id = ?2 AND client_session_id IS NULL",
            rusqlite::params![client_sid, &session.id],
        );
    }

    let endpoint = notify_endpoint(&session.id);
    let Some(stream) = notify_connect(&endpoint) else {
        return;
    };

    let request = serde_json::json!({ "refresh_roots": true });
    let _ = ipc_exchange(stream, &request);
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

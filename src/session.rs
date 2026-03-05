// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Session management for observability.
//!
//! Each Catenary instance creates a session that can be discovered and
//! monitored from other terminals via `catenary list` and `catenary monitor`.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Session metadata stored in info.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Unique session ID.
    pub id: String,
    /// Process ID of the Catenary instance.
    pub pid: u32,
    /// Path to the workspace root.
    pub workspace: String,
    /// When the session started.
    pub started_at: DateTime<Utc>,
    /// Name of the connected MCP client.
    #[serde(default)]
    pub client_name: Option<String>,
    /// Version of the connected MCP client.
    #[serde(default)]
    pub client_version: Option<String>,
}

/// An event that can be broadcast to listeners.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    /// When the event occurred.
    pub timestamp: DateTime<Utc>,
    /// The specific event data.
    #[serde(flatten)]
    pub kind: EventKind,
}

/// Types of session events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// Server state changed.
    ServerState {
        /// The language ID of the server.
        language: String,
        /// The new state.
        state: String,
    },
    /// Progress update from LSP server.
    Progress {
        /// The language ID of the server.
        language: String,
        /// The title of the progress operation.
        title: String,
        /// The optional progress message.
        message: Option<String>,
        /// The optional progress percentage (0-100).
        percentage: Option<u32>,
    },
    /// Progress completed.
    ProgressEnd {
        /// The language ID of the server.
        language: String,
    },
    /// Tool was called.
    ToolCall {
        /// The name of the tool called.
        tool: String,
        /// The optional file path involved.
        file: Option<String>,
        /// MCP request arguments (gated by `capture_tool_output`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params: Option<serde_json::Value>,
    },
    /// Tool call completed.
    ToolResult {
        /// The name of the tool called.
        tool: String,
        /// Whether the tool call was successful.
        success: bool,
        /// How long the tool call took in milliseconds.
        duration_ms: u64,
        /// Full tool output text (for TUI detail expansion).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        /// MCP request arguments echoed back (for TUI detail header).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params: Option<serde_json::Value>,
    },
    /// Diagnostics returned from notify hook.
    Diagnostics {
        /// File that was checked.
        file: String,
        /// Number of diagnostics found.
        count: usize,
        /// Compact diagnostics text with optional fix lines.
        preview: String,
    },
    /// Session started.
    Started,
    /// Session ending.
    Shutdown,
    /// Raw MCP message (incoming or outgoing).
    McpMessage {
        /// Direction of the message ("in" or "out").
        direction: String,
        /// The raw JSON-RPC message.
        message: serde_json::Value,
    },
}

/// Returns the base directory for session data.
pub fn sessions_dir() -> PathBuf {
    let state_dir = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    state_dir.join("catenary").join("sessions")
}

/// An active session that broadcasts events.
pub struct Session {
    /// Metadata about the session.
    pub info: SessionInfo,

    dir: PathBuf,

    events_file: Arc<Mutex<File>>,

    /// Path to the notify IPC endpoint (if started).
    socket_path: Option<PathBuf>,
}

impl Session {
    /// Create a new session.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The session directory cannot be created.
    /// - Metadata or event files cannot be created.
    pub fn create(workspace: &str) -> Result<Self> {
        let id = Self::generate_id();

        let sessions_base = sessions_dir();

        let session_dir = sessions_base.join(&id);

        fs::create_dir_all(&session_dir)
            .with_context(|| format!("Failed to create session dir: {}", session_dir.display()))?;

        let info = SessionInfo {
            id,

            pid: std::process::id(),

            workspace: workspace.to_string(),

            started_at: Utc::now(),

            client_name: None,

            client_version: None,
        };

        // Write info.json

        let info_path = session_dir.join("info.json");

        let info_file = File::create(&info_path)?;

        serde_json::to_writer_pretty(info_file, &info)?;

        // Create events.jsonl

        let events_path = session_dir.join("events.jsonl");

        let events_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&events_path)?;

        let session = Self {
            info,

            dir: session_dir,

            events_file: Arc::new(Mutex::new(events_file)),

            socket_path: None,
        };

        // Broadcast started event

        session.broadcast(EventKind::Started);

        Ok(session)
    }

    /// Generate a short unique session ID.
    fn generate_id() -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO)
            .as_millis();

        let pid = std::process::id();

        // Atomic counter guarantees uniqueness within the same process,
        // even when multiple sessions are created in the same millisecond.
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);

        format!("{:x}{:x}{:x}", u32::try_from(now).unwrap_or(0), pid, seq)
    }

    /// Returns the path to the notify IPC endpoint for this session.
    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        #[cfg(unix)]
        {
            self.dir.join("notify.sock")
        }
        #[cfg(windows)]
        {
            PathBuf::from(format!(r"\\.\pipe\catenary-{}", self.info.id))
        }
    }

    /// Records that the notify socket has been started, so it will be
    /// cleaned up on drop.
    pub fn set_socket_active(&mut self) {
        self.socket_path = Some(self.socket_path());
    }

    /// Update client info (called after MCP initialize).
    pub fn set_client_info(&mut self, name: &str, version: &str) {
        self.info.client_name = Some(name.to_string());

        self.info.client_version = Some(version.to_string());

        // Rewrite info.json

        let info_path = self.dir.join("info.json");

        if let Ok(file) = File::create(&info_path) {
            let _ = serde_json::to_writer_pretty(file, &self.info);
        }
    }

    /// Broadcast an event to listeners.
    pub fn broadcast(&self, kind: EventKind) {
        let event = SessionEvent {
            timestamp: Utc::now(),

            kind,
        };

        if let Ok(mut file) = self.events_file.lock()
            && let Ok(json) = serde_json::to_string(&event)
        {
            let _ = writeln!(file, "{json}");

            let _ = file.flush();
        }
    }

    /// Get a broadcaster that can be cloned and shared.
    #[must_use]
    pub fn broadcaster(&self) -> EventBroadcaster {
        EventBroadcaster {
            events_file: self.events_file.clone(),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Broadcast shutdown

        self.broadcast(EventKind::Shutdown);

        // Write a dead marker so observers can tell the session ended cleanly
        // even if the PID is later reused by another process.
        let _ = File::create(self.dir.join("dead"));

        // Clean up notify socket (Unix only — named pipes are kernel
        // objects cleaned up automatically when all handles close)
        #[cfg(unix)]
        if let Some(ref sock) = self.socket_path {
            let _ = fs::remove_file(sock);
        }

        // Session directory is intentionally retained for post-mortem inspection.
        // Pruning is handled separately based on log_retention_days.
    }
}

/// Cloneable broadcaster for sharing across components.
#[derive(Clone)]
pub struct EventBroadcaster {
    events_file: Arc<Mutex<File>>,
}

impl EventBroadcaster {
    /// Broadcast an event.
    pub fn send(&self, kind: EventKind) {
        let event = SessionEvent {
            timestamp: Utc::now(),
            kind,
        };

        if let Ok(mut file) = self.events_file.lock()
            && let Ok(json) = serde_json::to_string(&event)
        {
            let _ = writeln!(file, "{json}");
            let _ = file.flush();
        }
    }

    /// Create a no-op broadcaster (for when session is disabled).
    ///
    /// # Errors
    ///
    /// Returns an error if the null file cannot be opened or created.
    pub fn noop() -> Result<Self> {
        // Create a broadcaster that writes to /dev/null
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .or_else(|_| {
                // Fallback for non-Unix systems
                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(std::env::temp_dir().join(".catenary_null"))
            })?;
        Ok(Self {
            events_file: Arc::new(Mutex::new(file)),
        })
    }
}

/// List all sessions (active and inactive).
///
/// Returns a list of sessions and their status (true = active, false = dead).
///
/// # Errors
///
/// Returns an error if the sessions directory cannot be read.
pub fn list_sessions() -> Result<Vec<(SessionInfo, bool)>> {
    let sessions_base = sessions_dir();

    if !sessions_base.exists() {
        return Ok(vec![]);
    }

    let mut sessions = Vec::new();

    for entry in fs::read_dir(&sessions_base)? {
        let entry = entry?;
        let info_path = entry.path().join("info.json");

        if info_path.exists()
            && let Ok(file) = File::open(&info_path)
            && let Ok(info) = serde_json::from_reader::<_, SessionInfo>(file)
        {
            let alive = session_is_alive(&entry.path(), info.pid);
            sessions.push((info, alive));
        }
    }

    // Sort by start time (most recent first)
    sessions.sort_by(|(a, _), (b, _)| b.started_at.cmp(&a.started_at));

    Ok(sessions)
}

/// Get a specific session by ID.
///
/// Returns the session info and its status (true = active, false = dead).
///
/// # Errors
///
/// Returns an error if the session info file exists but cannot be read or parsed.
pub fn get_session(id: &str) -> Result<Option<(SessionInfo, bool)>> {
    let sessions_base = sessions_dir();
    let info_path = sessions_base.join(id).join("info.json");

    if !info_path.exists() {
        return Ok(None);
    }

    let file = File::open(&info_path)?;
    let info: SessionInfo = serde_json::from_reader(file)?;
    let session_dir = sessions_base.join(id);
    let alive = session_is_alive(&session_dir, info.pid);

    Ok(Some((info, alive)))
}

/// Monitor events from a session (blocking iterator).
///
/// # Errors
///
/// Returns an error if the session does not exist or the events file cannot be opened.
pub fn monitor_events(id: &str) -> Result<impl Iterator<Item = SessionEvent>> {
    let sessions_base = sessions_dir();
    let events_path = sessions_base.join(id).join("events.jsonl");

    if !events_path.exists() {
        anyhow::bail!("Session not found: {id}");
    }

    let file = File::open(&events_path)?;
    let reader = BufReader::new(file);

    Ok(reader.lines().filter_map(|line| {
        line.ok()
            .and_then(|l| serde_json::from_str::<SessionEvent>(&l).ok())
    }))
}

/// Tail events from a session (follows new events from the beginning).
///
/// # Errors
///
/// Returns an error if the session does not exist or the events file cannot be opened.
pub fn tail_events(id: &str) -> Result<TailReader> {
    let sessions_base = sessions_dir();
    let events_path = sessions_base.join(id).join("events.jsonl");

    if !events_path.exists() {
        anyhow::bail!("Session not found: {id}");
    }

    TailReader::new(events_path)
}

/// Tail only *new* events from a session (seeks to end of file first).
///
/// Use this when historical events have already been loaded separately
/// and you only want events written after this call.
///
/// # Errors
///
/// Returns an error if the session does not exist or the events file cannot be opened.
pub fn tail_events_new(id: &str) -> Result<TailReader> {
    let sessions_base = sessions_dir();
    let events_path = sessions_base.join(id).join("events.jsonl");

    if !events_path.exists() {
        anyhow::bail!("Session not found: {id}");
    }

    TailReader::new_from_end(events_path)
}

/// Reader that tails a file for new content.
pub struct TailReader {
    path: PathBuf,
    reader: BufReader<File>,
    last_size: u64,
}

impl TailReader {
    fn new(path: PathBuf) -> Result<Self> {
        let file = File::open(&path)?;
        let metadata = file.metadata()?;
        let reader = BufReader::new(file);

        Ok(Self {
            path,
            reader,
            last_size: metadata.len(),
        })
    }

    /// Create a reader positioned at the end of the file, so it only
    /// picks up events written after this call.
    fn new_from_end(path: PathBuf) -> Result<Self> {
        use std::io::{Seek, SeekFrom};

        let file = File::open(&path)?;
        let metadata = file.metadata()?;
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::End(0))?;

        Ok(Self {
            path,
            reader,
            last_size: metadata.len(),
        })
    }

    /// Read the next event if available. Returns `None` if no new event is currently in the file.
    ///
    /// # Errors
    ///
    /// Returns an error if reading from the file fails.
    pub fn try_next_event(&mut self) -> Result<Option<SessionEvent>> {
        use std::io::Seek;

        let mut line = String::new();
        let bytes_read = self.reader.read_line(&mut line)?;

        if bytes_read > 0 {
            let line = line.trim();
            if !line.is_empty() {
                // If parse fails, we just skip it (log error?) or return None?
                // Original code ignored parse errors implicitly by looping.
                // Let's return None if parse fails to avoid breaking the loop in caller,
                // but ideally we should log it. For now, let's match original behavior:
                // if parse ok -> return Some.
                if let Ok(event) = serde_json::from_str::<SessionEvent>(line) {
                    return Ok(Some(event));
                }
            }
            // Empty line or parse error: treat as "read something but no event"
            // We can return None (so caller sleeps) or recurse?
            // Let's return None. Caller will sleep 100ms and retry.
            return Ok(None);
        }

        // Check if file was truncated or if we should wait
        if let Ok(metadata) = fs::metadata(&self.path) {
            if metadata.len() < self.last_size {
                // File was truncated, reopen
                let file = File::open(&self.path)?;
                self.reader = BufReader::new(file);
                self.last_size = 0;
                // Retry reading immediately? Or just return None and let caller loop?
                // Return None.
                return Ok(None);
            }

            if metadata.len() > self.last_size {
                // File grew — reset BufReader's EOF state so
                // it reads new data on the next iteration.
                self.reader.stream_position()?;
            }

            self.last_size = metadata.len();
        } else {
            // File was deleted
            // We could return a special error or just let the caller handle it.
            // But if file is deleted, `run_monitor` usually exits.
            // Since we don't delete files anymore, this case is rare (manual deletion).
            // Let's return error to signal "Stop".
            anyhow::bail!("Events file deleted");
        }

        Ok(None)
    }
}

/// Get active languages for a session by reading its events.
///
/// # Errors
///
/// Returns an error if the events file exists but cannot be read.
pub fn active_languages(id: &str) -> Result<Vec<String>> {
    use std::collections::HashMap;

    let sessions_base = sessions_dir();
    let events_path = sessions_base.join(id).join("events.jsonl");

    if !events_path.exists() {
        return Ok(vec![]);
    }

    let file = File::open(&events_path)?;
    let reader = BufReader::new(file);

    // Track server states: language -> state
    let mut states: HashMap<String, String> = HashMap::new();

    for line in reader.lines().map_while(Result::ok) {
        if let Ok(event) = serde_json::from_str::<SessionEvent>(&line)
            && let EventKind::ServerState { language, state } = event.kind
        {
            if state == "Dead" {
                states.remove(&language);
            } else {
                states.insert(language, state);
            }
        }
    }

    let mut languages: Vec<String> = states.keys().cloned().collect();
    languages.sort();
    Ok(languages)
}

/// Remove dead session directories older than the configured retention period.
///
/// - `retention_days == -1`: retain forever (no-op).
/// - `retention_days == 0`: remove all dead sessions regardless of age.
/// - `retention_days > 0`: remove dead sessions whose `started_at` is older
///   than `retention_days` days ago.
///
/// Active sessions are never pruned.
///
/// # Errors
///
/// Returns an error if the sessions directory cannot be read.  Individual
/// session removal failures are logged and skipped.
pub fn prune_sessions(retention_days: i64) -> Result<usize> {
    if retention_days < 0 {
        return Ok(0);
    }

    let sessions_base = sessions_dir();
    if !sessions_base.exists() {
        return Ok(0);
    }

    let cutoff = if retention_days == 0 {
        // Remove all dead sessions — use a far-future cutoff so everything qualifies.
        Utc::now() + chrono::Duration::days(1)
    } else {
        Utc::now() - chrono::Duration::days(retention_days)
    };

    let mut removed = 0usize;

    for entry in fs::read_dir(&sessions_base)? {
        let entry = entry?;
        let dir = entry.path();

        let info_path = dir.join("info.json");
        if !info_path.exists() {
            continue;
        }

        // Parse session info to get PID and age.
        let Ok(file) = File::open(&info_path) else {
            continue;
        };
        let Ok(info) = serde_json::from_reader::<_, SessionInfo>(file) else {
            continue;
        };

        // Never prune active sessions.
        if session_is_alive(&dir, info.pid) {
            continue;
        }

        // Only prune if older than cutoff.
        if info.started_at < cutoff {
            if let Err(e) = fs::remove_dir_all(&dir) {
                tracing::warn!("Failed to prune session {}: {e}", dir.display());
            } else {
                removed += 1;
            }
        }
    }

    Ok(removed)
}

/// Determine whether a session is alive.
///
/// A session is dead if it wrote a `dead` marker on shutdown, or if its
/// recorded PID is no longer running (crash / SIGKILL recovery).
fn session_is_alive(session_dir: &std::path::Path, pid: u32) -> bool {
    if session_dir.join("dead").exists() {
        return false;
    }
    is_process_alive(pid)
}

/// Check if a process is still running.
fn is_process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        // On Linux, checking /proc/<pid> is safe and doesn't require unsafe blocks.
        std::path::Path::new("/proc").join(pid.to_string()).exists()
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // On other Unix systems, we use the kill command with signal 0.
        // This is safe but slightly slower than a syscall.
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[cfg(not(unix))]
    {
        // On non-Unix, assume alive (could use platform-specific APIs).
        let _ = pid;
        true
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use anyhow::Result;

    #[test]
    fn test_session_create_and_list() -> Result<()> {
        let session = Session::create("/tmp/test-workspace")?;
        let id = session.info.id.clone();

        // Should appear in list
        let sessions = list_sessions()?;
        assert!(sessions.iter().any(|(s, _)| s.id == id));

        // Should be retrievable
        let found = get_session(&id)?;
        let (found_session, _) = found.expect("session should be retrievable");
        assert_eq!(found_session.workspace, "/tmp/test-workspace");

        // Drop session
        drop(session);

        // Should NOT be cleaned up immediately anymore (changed behavior)
        // But get_session should still return it (as dead)
        let found = get_session(&id)?;
        let (_, alive) = found.expect("session should exist after drop");
        assert!(!alive, "Session should be dead after drop");

        // Manual cleanup
        let _ = fs::remove_dir_all(sessions_dir().join(id));

        Ok(())
    }

    #[test]
    fn test_event_broadcast() -> Result<()> {
        let session = Session::create("/tmp/test-events")?;
        let id = session.info.id.clone();

        session.broadcast(EventKind::ServerState {
            language: "rust".to_string(),
            state: "Busy".to_string(),
        });

        session.broadcast(EventKind::Progress {
            language: "rust".to_string(),
            title: "Loading".to_string(),
            message: Some("crates".to_string()),
            percentage: Some(50),
        });

        // Read events back
        assert!(monitor_events(&id)?.count() >= 2); // Started + our events

        drop(session);
        Ok(())
    }

    #[test]
    fn test_active_languages_empty() -> Result<()> {
        let session = Session::create("/tmp/test-langs-empty")?;
        let id = session.info.id.clone();

        // No server state events, should return empty
        let langs = active_languages(&id)?;
        assert!(langs.is_empty());

        drop(session);
        Ok(())
    }

    #[test]
    fn test_active_languages_tracks_server_state() -> Result<()> {
        let session = Session::create("/tmp/test-langs-state")?;
        let id = session.info.id.clone();

        session.broadcast(EventKind::ServerState {
            language: "rust".to_string(),
            state: "Initializing".to_string(),
        });

        session.broadcast(EventKind::ServerState {
            language: "rust".to_string(),
            state: "Ready".to_string(),
        });

        let langs = active_languages(&id)?;
        assert_eq!(langs, vec!["rust"]);

        drop(session);
        Ok(())
    }

    #[test]
    fn test_active_languages_removes_dead() -> Result<()> {
        let session = Session::create("/tmp/test-langs-dead")?;
        let id = session.info.id.clone();

        session.broadcast(EventKind::ServerState {
            language: "rust".to_string(),
            state: "Ready".to_string(),
        });

        session.broadcast(EventKind::ServerState {
            language: "rust".to_string(),
            state: "Dead".to_string(),
        });

        let langs = active_languages(&id)?;
        assert!(langs.is_empty());

        drop(session);
        Ok(())
    }

    #[test]
    fn test_active_languages_multiple_languages() -> Result<()> {
        let session = Session::create("/tmp/test-langs-multi")?;
        let id = session.info.id.clone();

        session.broadcast(EventKind::ServerState {
            language: "rust".to_string(),
            state: "Ready".to_string(),
        });

        session.broadcast(EventKind::ServerState {
            language: "python".to_string(),
            state: "Ready".to_string(),
        });

        session.broadcast(EventKind::ServerState {
            language: "typescript".to_string(),
            state: "Initializing".to_string(),
        });

        let langs = active_languages(&id)?;
        assert_eq!(langs, vec!["python", "rust", "typescript"]);

        drop(session);
        Ok(())
    }

    /// Helper: create a dead session, optionally backdated.
    fn create_dead_session(workspace: &str, backdate_days: Option<i64>) -> Result<PathBuf> {
        let session = Session::create(workspace)?;
        let id = session.info.id.clone();
        let dir = sessions_dir().join(&id);
        drop(session);

        if let Some(days) = backdate_days {
            let info_path = dir.join("info.json");
            let file = File::open(&info_path)?;
            let mut info: SessionInfo = serde_json::from_reader(file)?;
            info.started_at = Utc::now() - chrono::Duration::days(days);
            let file = File::create(&info_path)?;
            serde_json::to_writer_pretty(file, &info)?;
        }
        Ok(dir)
    }

    /// Single sequential test covering all `prune_sessions` behaviours.
    ///
    /// These must run in sequence because `prune_sessions` operates on the
    /// shared `sessions_dir()` and parallel execution causes interference.
    #[test]
    fn test_prune_sessions() -> Result<()> {
        // -- retention=-1 retains forever --
        let dir_forever = create_dead_session("/tmp/test-prune-forever", Some(365))?;
        let removed = prune_sessions(-1)?;
        assert_eq!(removed, 0, "retention=-1 should never prune");
        assert!(dir_forever.exists());
        let _ = fs::remove_dir_all(&dir_forever);

        // -- retention=7 keeps recent, removes old --
        let dir_recent = create_dead_session("/tmp/test-prune-recent", None)?;
        let dir_old = create_dead_session("/tmp/test-prune-old", Some(10))?;

        let _ = prune_sessions(7)?;
        assert!(
            dir_recent.exists(),
            "recent dead session should survive prune"
        );
        assert!(!dir_old.exists(), "old dead session should be pruned");
        let _ = fs::remove_dir_all(&dir_recent);

        // -- retention=0 removes all dead --
        let dir_zero = create_dead_session("/tmp/test-prune-zero", None)?;
        let _ = prune_sessions(0)?;
        assert!(
            !dir_zero.exists(),
            "dead session should be removed with retention=0"
        );

        Ok(())
    }
}

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
use tracing::warn;

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
    },
    /// Tool call completed.
    ToolResult {
        /// The name of the tool called.
        tool: String,
        /// Whether the tool call was successful.
        success: bool,
        /// How long the tool call took in milliseconds.
        duration_ms: u64,
    },
    /// Diagnostics returned from notify hook.
    Diagnostics {
        /// File that was checked.
        file: String,
        /// Number of diagnostics found.
        count: usize,
        /// Short preview of the first diagnostic.
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
        use std::time::{SystemTime, UNIX_EPOCH};

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO)
            .as_millis();

        let pid = std::process::id();

        // Use thread ID to avoid collisions in tests

        let tid = format!("{:?}", std::thread::current().id());

        // Simple hash of tid to keep it short

        let tid_hash = tid
            .bytes()
            .fold(0u32, |acc, x| acc.wrapping_add(u32::from(x)));

        format!(
            "{:x}{:x}{:x}",
            u32::try_from(now).unwrap_or(0),
            pid,
            tid_hash
        )
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

        // Clean up notify socket (Unix only — named pipes are kernel
        // objects cleaned up automatically when all handles close)
        #[cfg(unix)]
        if let Some(ref sock) = self.socket_path {
            let _ = fs::remove_file(sock);
        }

        // Clean up session directory

        if let Err(e) = fs::remove_dir_all(&self.dir) {
            warn!("Failed to clean up session directory: {}", e);
        }
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

/// List all active sessions.
///
/// # Errors
///
/// Returns an error if the sessions directory cannot be read.
pub fn list_sessions() -> Result<Vec<SessionInfo>> {
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
            // Check if process is still alive
            if is_process_alive(info.pid) {
                sessions.push(info);
            } else {
                // Clean up dead session
                warn!("Cleaning up dead session {} (pid {})", info.id, info.pid);
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }

    // Sort by start time (most recent first)
    sessions.sort_by(|a, b| b.started_at.cmp(&a.started_at));

    Ok(sessions)
}

/// Get a specific session by ID.
///
/// # Errors
///
/// Returns an error if the session info file exists but cannot be read or parsed.
pub fn get_session(id: &str) -> Result<Option<SessionInfo>> {
    let sessions_base = sessions_dir();
    let info_path = sessions_base.join(id).join("info.json");

    if !info_path.exists() {
        return Ok(None);
    }

    let file = File::open(&info_path)?;
    let info: SessionInfo = serde_json::from_reader(file)?;

    if is_process_alive(info.pid) {
        Ok(Some(info))
    } else {
        // Clean up dead session
        let _ = fs::remove_dir_all(sessions_base.join(id));
        Ok(None)
    }
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

/// Tail events from a session (follows new events).
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

    /// Read the next event, blocking if necessary.
    ///
    /// # Errors
    ///
    /// Returns an error if reading from the file fails.
    pub fn next_event(&mut self) -> Result<Option<SessionEvent>> {
        use std::io::Seek;

        loop {
            let mut line = String::new();
            let bytes_read = self.reader.read_line(&mut line)?;

            if bytes_read > 0 {
                let line = line.trim();
                if !line.is_empty()
                    && let Ok(event) = serde_json::from_str::<SessionEvent>(line)
                {
                    return Ok(Some(event));
                }
            } else {
                // Check if file was truncated or if we should wait
                if let Ok(metadata) = fs::metadata(&self.path) {
                    if metadata.len() < self.last_size {
                        // File was truncated, reopen
                        let file = File::open(&self.path)?;
                        self.reader = BufReader::new(file);
                        self.last_size = 0;
                        continue;
                    }

                    if metadata.len() > self.last_size {
                        // File grew — reset BufReader's EOF state so
                        // it reads new data on the next iteration.
                        self.reader.stream_position()?;
                    }

                    self.last_size = metadata.len();
                } else {
                    // File was deleted, session ended
                    return Ok(None);
                }

                // Wait a bit before checking again
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
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
mod tests {
    use super::*;
    use anyhow::{Context, Result};

    #[test]
    fn test_session_create_and_list() -> Result<()> {
        let session = Session::create("/tmp/test-workspace")?;
        let id = session.info.id.clone();

        // Should appear in list
        let sessions = list_sessions()?;
        assert!(sessions.iter().any(|s| s.id == id));

        // Should be retrievable
        let found = get_session(&id)?;
        let found_session = found.context("missing session")?;
        assert_eq!(found_session.workspace, "/tmp/test-workspace");

        // Drop session
        drop(session);

        // Should be cleaned up
        let found = get_session(&id)?;
        assert!(found.is_none());
        Ok(())
    }

    #[test]
    fn test_event_broadcast() -> Result<()> {
        let session = Session::create("/tmp/test-events")?;
        let id = session.info.id.clone();

        session.broadcast(EventKind::ServerState {
            language: "rust".to_string(),
            state: "Indexing".to_string(),
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
}

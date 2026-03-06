// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Session management for observability.
//!
//! Each Catenary instance creates a session that can be discovered and
//! monitored from other terminals via `catenary list` and `catenary monitor`.
//!
//! Sessions are stored in SQLite via the [`crate::db`] module.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Session metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Unique session ID.
    pub id: String,
    /// Process ID of the Catenary instance.
    pub pid: u32,
    /// Display name (comma-joined workspace roots).
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

/// Returns the base directory for session runtime artifacts (notify sockets).
#[must_use]
pub fn sessions_dir() -> PathBuf {
    crate::db::state_dir().join("catenary").join("sessions")
}

/// An active session that broadcasts events.
pub struct Session {
    /// Metadata about the session.
    pub info: SessionInfo,

    conn: Arc<Mutex<Connection>>,
    broadcaster: EventBroadcaster,

    /// Path to the notify IPC endpoint (if started).
    socket_path: Option<PathBuf>,
}

impl Session {
    /// Create a new session.
    ///
    /// Opens a database connection internally. For explicit connection
    /// management, use [`Session::create_with_conn`].
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or the session
    /// cannot be inserted.
    pub fn create(workspace: &str) -> Result<Self> {
        let conn = Arc::new(Mutex::new(crate::db::open_and_migrate()?));
        Self::create_with_conn(workspace, conn)
    }

    /// Create a new session with an existing database connection.
    ///
    /// # Errors
    ///
    /// Returns an error if the session cannot be inserted into the database
    /// or the socket directory cannot be created.
    pub fn create_with_conn(workspace: &str, conn: Arc<Mutex<Connection>>) -> Result<Self> {
        let id = Self::generate_id();
        let started_at = Utc::now();

        let info = SessionInfo {
            id,
            pid: std::process::id(),
            workspace: workspace.to_string(),
            started_at,
            client_name: None,
            client_version: None,
        };

        {
            let c = conn.lock().map_err(|_| anyhow::anyhow!("mutex poisoned"))?;
            c.execute(
                "INSERT INTO sessions (id, pid, display_name, started_at, alive) \
                 VALUES (?1, ?2, ?3, ?4, 1)",
                rusqlite::params![&info.id, info.pid, workspace, started_at.to_rfc3339()],
            )
            .context("failed to insert session")?;

            for root in workspace
                .split(',')
                .map(str::trim)
                .filter(|r| !r.is_empty())
            {
                c.execute(
                    "INSERT INTO workspace_roots (session_id, root_path) VALUES (?1, ?2)",
                    rusqlite::params![&info.id, root],
                )?;
            }
        }

        // Create socket directory (for notify.sock)
        let socket_dir = sessions_dir().join(&info.id);
        std::fs::create_dir_all(&socket_dir)
            .with_context(|| format!("failed to create socket dir: {}", socket_dir.display()))?;

        let broadcaster = EventBroadcaster {
            inner: BroadcasterInner::Live {
                conn: conn.clone(),
                session_id: info.id.clone(),
            },
        };

        let session = Self {
            info,
            conn,
            broadcaster,
            socket_path: None,
        };

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
            sessions_dir().join(&self.info.id).join("notify.sock")
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

        if let Ok(c) = self.conn.lock() {
            let _ = c.execute(
                "UPDATE sessions SET client_name = ?1, client_version = ?2 WHERE id = ?3",
                rusqlite::params![name, version, &self.info.id],
            );
        }
    }

    /// Broadcast an event to listeners.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "public API takes ownership by convention"
    )]
    pub fn broadcast(&self, kind: EventKind) {
        self.broadcaster.send(kind);
    }

    /// Get a broadcaster that can be cloned and shared.
    #[must_use]
    pub fn broadcaster(&self) -> EventBroadcaster {
        self.broadcaster.clone()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.broadcast(EventKind::Shutdown);

        if let Ok(c) = self.conn.lock() {
            let _ = c.execute(
                "UPDATE sessions SET alive = 0, ended_at = ?1 WHERE id = ?2",
                rusqlite::params![Utc::now().to_rfc3339(), &self.info.id],
            );
        }

        // Clean up notify socket (Unix only — named pipes are kernel
        // objects cleaned up automatically when all handles close)
        #[cfg(unix)]
        if let Some(ref sock) = self.socket_path {
            let _ = std::fs::remove_file(sock);
        }

        // Remove socket directory (only succeeds if empty)
        let socket_dir = sessions_dir().join(&self.info.id);
        let _ = std::fs::remove_dir(&socket_dir);
    }
}

/// Cloneable broadcaster for sharing across components.
#[derive(Clone)]
pub struct EventBroadcaster {
    inner: BroadcasterInner,
}

#[derive(Clone)]
enum BroadcasterInner {
    Live {
        conn: Arc<Mutex<Connection>>,
        session_id: String,
    },
    Noop,
}

impl EventBroadcaster {
    /// Broadcast an event.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "public API takes ownership by convention"
    )]
    pub fn send(&self, kind: EventKind) {
        if let BroadcasterInner::Live { conn, session_id } = &self.inner {
            let timestamp = Utc::now();
            let kind_tag = event_kind_tag(&kind);

            if let Ok(payload) = serde_json::to_string(&kind)
                && let Ok(c) = conn.lock()
            {
                let _ = c.execute(
                    "INSERT INTO events (session_id, timestamp, kind, payload) \
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![session_id, timestamp.to_rfc3339(), kind_tag, payload],
                );
            }
        }
    }

    /// Create a no-op broadcaster (for when session is disabled).
    #[must_use]
    pub const fn noop() -> Self {
        Self {
            inner: BroadcasterInner::Noop,
        }
    }
}

// ── Event tailing (SQLite-backed) ────────────────────────────────────

/// Polls the events table for new events, replacing the file-based
/// `TailReader` that was removed in the `SQLite` migration.
pub struct SqliteEventTail {
    conn: Connection,
    session_id: String,
    last_id: i64,
}

impl SqliteEventTail {
    /// Read the next event if available. Returns `None` if no new event yet.
    ///
    /// # Errors
    ///
    /// Returns an error if reading from the database fails.
    pub fn try_next_event(&mut self) -> Result<Option<SessionEvent>> {
        let result = self.conn.query_row(
            "SELECT id, timestamp, payload FROM events \
             WHERE session_id = ?1 AND id > ?2 ORDER BY id LIMIT 1",
            rusqlite::params![&self.session_id, self.last_id],
            |row| {
                let id: i64 = row.get(0)?;
                let ts: String = row.get(1)?;
                let payload: String = row.get(2)?;
                Ok((id, ts, payload))
            },
        );

        match result {
            Ok((id, ts, payload)) => {
                self.last_id = id;
                let timestamp = DateTime::parse_from_rfc3339(&ts)
                    .with_context(|| format!("invalid event timestamp: {ts}"))?
                    .with_timezone(&Utc);
                let kind: EventKind =
                    serde_json::from_str(&payload).context("invalid event payload")?;
                Ok(Some(SessionEvent { timestamp, kind }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

// ── Free functions ───────────────────────────────────────────────────

/// Raw row from the sessions table (avoids complex tuple types).
struct SessionRow {
    id: String,
    pid: u32,
    display_name: String,
    client_name: Option<String>,
    client_version: Option<String>,
    started_at_str: String,
    db_alive: bool,
}

/// List all sessions (active and inactive).
///
/// Opens a database connection internally. For explicit connection
/// management, use [`list_sessions_with_conn`].
///
/// # Errors
///
/// Returns an error if the database cannot be opened or queried.
pub fn list_sessions() -> Result<Vec<(SessionInfo, bool)>> {
    let conn = crate::db::open_and_migrate()?;
    list_sessions_with_conn(&conn)
}

/// List all sessions using an existing database connection.
///
/// Returns a list of sessions and their status (true = active, false = dead).
/// Crashed sessions (PID gone but `alive` flag set) are marked dead in the DB.
///
/// # Errors
///
/// Returns an error if the database cannot be queried.
pub fn list_sessions_with_conn(conn: &Connection) -> Result<Vec<(SessionInfo, bool)>> {
    // Collect raw rows first to release the statement borrow.
    let rows = {
        let mut stmt = conn.prepare(
            "SELECT id, pid, display_name, client_name, client_version, started_at, alive \
             FROM sessions ORDER BY started_at DESC",
        )?;
        let mut r = stmt.query([])?;
        let mut rows = Vec::new();
        while let Some(row) = r.next()? {
            rows.push(SessionRow {
                id: row.get(0)?,
                pid: row.get(1)?,
                display_name: row.get(2)?,
                client_name: row.get(3)?,
                client_version: row.get(4)?,
                started_at_str: row.get(5)?,
                db_alive: row.get(6)?,
            });
        }
        rows
    };

    let mut sessions = Vec::with_capacity(rows.len());
    for r in rows {
        let SessionRow {
            id,
            pid,
            display_name,
            client_name,
            client_version,
            started_at_str,
            db_alive,
        } = r;
        let started_at = DateTime::parse_from_rfc3339(&started_at_str)
            .with_context(|| format!("invalid started_at: {started_at_str}"))?
            .with_timezone(&Utc);

        let alive = if db_alive {
            if is_process_alive(pid) {
                true
            } else {
                // Process crashed — mark dead in DB.
                let _ = conn.execute(
                    "UPDATE sessions SET alive = 0, ended_at = ?1 WHERE id = ?2",
                    rusqlite::params![Utc::now().to_rfc3339(), &id],
                );
                false
            }
        } else {
            false
        };

        sessions.push((
            SessionInfo {
                id,
                pid,
                workspace: display_name,
                started_at,
                client_name,
                client_version,
            },
            alive,
        ));
    }

    Ok(sessions)
}

/// Get a specific session by ID.
///
/// Opens a database connection internally. For explicit connection
/// management, use [`get_session_with_conn`].
///
/// # Errors
///
/// Returns an error if the database cannot be opened or queried.
pub fn get_session(id: &str) -> Result<Option<(SessionInfo, bool)>> {
    let conn = crate::db::open_and_migrate()?;
    get_session_with_conn(&conn, id)
}

/// Get a specific session by ID using an existing database connection.
///
/// Returns the session info and its status (true = active, false = dead).
///
/// # Errors
///
/// Returns an error if the database cannot be queried.
pub fn get_session_with_conn(conn: &Connection, id: &str) -> Result<Option<(SessionInfo, bool)>> {
    let result = conn.query_row(
        "SELECT id, pid, display_name, client_name, client_version, started_at, alive \
         FROM sessions WHERE id = ?1",
        [id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, bool>(6)?,
            ))
        },
    );

    match result {
        Ok((sid, pid, display_name, client_name, client_version, started_at_str, db_alive)) => {
            let started_at = DateTime::parse_from_rfc3339(&started_at_str)
                .with_context(|| format!("invalid started_at: {started_at_str}"))?
                .with_timezone(&Utc);

            let alive = if db_alive {
                if is_process_alive(pid) {
                    true
                } else {
                    let _ = conn.execute(
                        "UPDATE sessions SET alive = 0, ended_at = ?1 WHERE id = ?2",
                        rusqlite::params![Utc::now().to_rfc3339(), &sid],
                    );
                    false
                }
            } else {
                false
            };

            Ok(Some((
                SessionInfo {
                    id: sid,
                    pid,
                    workspace: display_name,
                    started_at,
                    client_name,
                    client_version,
                },
                alive,
            )))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Monitor events from a session (returns all historical events).
///
/// Opens a database connection internally. For explicit connection
/// management, use [`monitor_events_with_conn`].
///
/// # Errors
///
/// Returns an error if the database cannot be opened or queried.
pub fn monitor_events(id: &str) -> Result<Vec<SessionEvent>> {
    let conn = crate::db::open_and_migrate()?;
    monitor_events_with_conn(&conn, id)
}

/// Monitor events from a session using an existing database connection.
///
/// Returns all historical events for the given session.
///
/// # Errors
///
/// Returns an error if the database cannot be queried.
pub fn monitor_events_with_conn(conn: &Connection, id: &str) -> Result<Vec<SessionEvent>> {
    let mut stmt =
        conn.prepare("SELECT timestamp, payload FROM events WHERE session_id = ?1 ORDER BY id")?;
    let mut rows = stmt.query([id])?;
    let mut events = Vec::new();

    while let Some(row) = rows.next()? {
        let ts: String = row.get(0)?;
        let payload: String = row.get(1)?;

        if let Ok(timestamp) = DateTime::parse_from_rfc3339(&ts)
            && let Ok(kind) = serde_json::from_str::<EventKind>(&payload)
        {
            events.push(SessionEvent {
                timestamp: timestamp.with_timezone(&Utc),
                kind,
            });
        }
    }

    Ok(events)
}

/// Tail events from a session (follows new events from the beginning).
///
/// Opens a database connection internally. For explicit connection
/// management, use [`tail_events_with_conn`].
///
/// # Errors
///
/// Returns an error if the session does not exist or the database cannot be opened.
pub fn tail_events(id: &str) -> Result<SqliteEventTail> {
    let conn = crate::db::open()?;
    tail_events_with_conn(conn, id)
}

/// Tail events from a session using an existing database connection.
///
/// The connection is moved into the returned [`SqliteEventTail`] for polling.
///
/// # Errors
///
/// Returns an error if the session does not exist.
pub fn tail_events_with_conn(conn: Connection, id: &str) -> Result<SqliteEventTail> {
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sessions WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap_or(false);

    if !exists {
        anyhow::bail!("Session not found: {id}");
    }

    Ok(SqliteEventTail {
        conn,
        session_id: id.to_string(),
        last_id: 0,
    })
}

/// Tail only *new* events from a session (starts from current end).
///
/// Use this when historical events have already been loaded separately
/// and you only want events written after this call.
///
/// Opens a database connection internally. For explicit connection
/// management, use [`tail_events_new_with_conn`].
///
/// # Errors
///
/// Returns an error if the session does not exist or the database cannot be opened.
pub fn tail_events_new(id: &str) -> Result<SqliteEventTail> {
    let conn = crate::db::open()?;
    tail_events_new_with_conn(conn, id)
}

/// Tail only *new* events from a session using an existing database connection.
///
/// The connection is moved into the returned [`SqliteEventTail`] for polling.
///
/// # Errors
///
/// Returns an error if the database cannot be queried.
pub fn tail_events_new_with_conn(conn: Connection, id: &str) -> Result<SqliteEventTail> {
    let last_id: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(id), 0) FROM events WHERE session_id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    Ok(SqliteEventTail {
        conn,
        session_id: id.to_string(),
        last_id,
    })
}

/// Get active languages for a session by reading its events.
///
/// Opens a database connection internally. For explicit connection
/// management, use [`active_languages_with_conn`].
///
/// # Errors
///
/// Returns an error if the database cannot be opened or queried.
pub fn active_languages(id: &str) -> Result<Vec<String>> {
    let conn = crate::db::open_and_migrate()?;
    active_languages_with_conn(&conn, id)
}

/// Get active languages for a session using an existing database connection.
///
/// # Errors
///
/// Returns an error if the database cannot be queried.
pub fn active_languages_with_conn(conn: &Connection, id: &str) -> Result<Vec<String>> {
    use std::collections::HashMap;

    let mut stmt = conn.prepare(
        "SELECT payload FROM events WHERE session_id = ?1 AND kind = 'server_state' ORDER BY id",
    )?;
    let mut rows = stmt.query([id])?;
    let mut states: HashMap<String, String> = HashMap::new();

    while let Some(row) = rows.next()? {
        let payload: String = row.get(0)?;
        if let Ok(EventKind::ServerState { language, state }) =
            serde_json::from_str::<EventKind>(&payload)
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

/// Remove dead sessions older than the configured retention period.
///
/// Opens a database connection internally. For explicit connection
/// management, use [`prune_sessions_with_conn`].
///
/// # Errors
///
/// Returns an error if the database cannot be opened or queried.
pub fn prune_sessions(retention_days: i64) -> Result<usize> {
    if retention_days < 0 {
        return Ok(0);
    }
    let conn = crate::db::open_and_migrate()?;
    prune_sessions_with_conn(&conn, retention_days)
}

/// Remove dead sessions older than the configured retention period
/// using an existing database connection.
///
/// - `retention_days == -1`: retain forever (no-op).
/// - `retention_days == 0`: remove all dead sessions regardless of age.
/// - `retention_days > 0`: remove dead sessions whose `started_at` is older
///   than `retention_days` days ago.
///
/// Active sessions are never pruned. Crashed sessions (PID gone) are
/// detected and marked dead before pruning.
///
/// # Errors
///
/// Returns an error if the database cannot be queried.
pub fn prune_sessions_with_conn(conn: &Connection, retention_days: i64) -> Result<usize> {
    if retention_days < 0 {
        return Ok(0);
    }

    // Detect crashed sessions (alive in DB but PID gone).
    let crashed: Vec<String> = {
        let mut stmt = conn.prepare("SELECT id, pid FROM sessions WHERE alive = 1")?;
        let mut rows = stmt.query([])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let pid: u32 = row.get(1)?;
            if !is_process_alive(pid) {
                ids.push(id);
            }
        }
        ids
    };

    let ended_at = Utc::now().to_rfc3339();
    for id in &crashed {
        let _ = conn.execute(
            "UPDATE sessions SET alive = 0, ended_at = ?1 WHERE id = ?2",
            rusqlite::params![&ended_at, id],
        );
    }

    let cutoff = if retention_days == 0 {
        // Remove all dead sessions — use a far-future cutoff.
        Utc::now() + chrono::Duration::days(1)
    } else {
        Utc::now() - chrono::Duration::days(retention_days)
    };

    let removed = conn.execute(
        "DELETE FROM sessions WHERE alive = 0 AND started_at < ?1",
        rusqlite::params![cutoff.to_rfc3339()],
    )?;

    Ok(removed)
}

/// Delete a session and all its associated data.
///
/// Opens a database connection internally. For explicit connection
/// management, use [`delete_session_data_with_conn`].
///
/// # Errors
///
/// Returns an error if the database cannot be opened or the delete fails.
pub fn delete_session_data(id: &str) -> Result<()> {
    let conn = crate::db::open_and_migrate()?;
    delete_session_data_with_conn(&conn, id)
}

/// Delete a session and all its associated data using an existing database
/// connection.
///
/// # Errors
///
/// Returns an error if the delete fails.
pub fn delete_session_data_with_conn(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM sessions WHERE id = ?1", [id])?;

    // Clean up socket directory if it exists.
    let socket_dir = sessions_dir().join(id);
    let _ = std::fs::remove_dir_all(&socket_dir);

    Ok(())
}

// ── Private helpers ──────────────────────────────────────────────────

/// Returns the serde tag for an event kind (used as the `kind` column value).
const fn event_kind_tag(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::ServerState { .. } => "server_state",
        EventKind::Progress { .. } => "progress",
        EventKind::ProgressEnd { .. } => "progress_end",
        EventKind::ToolCall { .. } => "tool_call",
        EventKind::ToolResult { .. } => "tool_result",
        EventKind::Diagnostics { .. } => "diagnostics",
        EventKind::Started => "started",
        EventKind::Shutdown => "shutdown",
        EventKind::McpMessage { .. } => "mcp_message",
    }
}

/// Check if a process is still running.
#[must_use]
pub fn is_process_alive(pid: u32) -> bool {
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
    use std::sync::Arc;

    /// Open an isolated test database in a tempdir.
    /// Returns `(TempDir, PathBuf, Connection)` — the tempdir guard must
    /// be held for the lifetime of the connection.
    fn test_db() -> (tempfile::TempDir, PathBuf, Connection) {
        let dir = tempfile::tempdir().expect("failed to create tempdir for test DB");
        let path = dir.path().join("catenary").join("catenary.db");
        let conn = crate::db::open_and_migrate_at(&path).expect("failed to open test DB");
        (dir, path, conn)
    }

    /// Create a session backed by the database at `db_path`.
    fn create_session(db_path: &std::path::Path, workspace: &str) -> Result<Session> {
        let arc = Arc::new(std::sync::Mutex::new(crate::db::open_and_migrate_at(
            db_path,
        )?));
        Session::create_with_conn(workspace, arc)
    }

    #[test]
    fn test_session_create_and_list() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let session = create_session(&path, "/tmp/test-workspace")?;
        let id = session.info.id.clone();

        // Should appear in list
        let sessions = list_sessions_with_conn(&conn)?;
        assert!(sessions.iter().any(|(s, _)| s.id == id));

        // Should be retrievable
        let found = get_session_with_conn(&conn, &id)?;
        let (found_session, _) = found.expect("session should be retrievable");
        assert_eq!(found_session.workspace, "/tmp/test-workspace");

        // Drop session
        drop(session);

        // get_session should still return it (as dead)
        let found = get_session_with_conn(&conn, &id)?;
        let (_, alive) = found.expect("session should exist after drop");
        assert!(!alive, "Session should be dead after drop");

        // Clean up
        delete_session_data_with_conn(&conn, &id)?;

        Ok(())
    }

    #[test]
    fn test_event_broadcast() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let session = create_session(&path, "/tmp/test-events")?;
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

        // Read events back (Started + 2 broadcast events = at least 3)
        assert!(monitor_events_with_conn(&conn, &id)?.len() >= 3);

        drop(session);
        delete_session_data_with_conn(&conn, &id)?;
        Ok(())
    }

    #[test]
    fn test_event_broadcast_serialization() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let session = create_session(&path, "/tmp/test-serialization")?;
        let id = session.info.id.clone();

        session.broadcast(EventKind::ToolCall {
            tool: "grep".to_string(),
            file: Some("/tmp/test.rs".to_string()),
            params: None,
        });

        let events = monitor_events_with_conn(&conn, &id)?;
        let tool_call = events
            .iter()
            .find(|e| matches!(&e.kind, EventKind::ToolCall { .. }))
            .expect("ToolCall event should be present");

        if let EventKind::ToolCall { tool, file, .. } = &tool_call.kind {
            assert_eq!(tool, "grep");
            assert_eq!(file.as_deref(), Some("/tmp/test.rs"));
        }

        drop(session);
        delete_session_data_with_conn(&conn, &id)?;
        Ok(())
    }

    #[test]
    fn test_session_set_client_info() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let mut session = create_session(&path, "/tmp/test-client-info")?;
        let id = session.info.id.clone();

        session.set_client_info("claude-code", "1.0.0");

        let found = get_session_with_conn(&conn, &id)?;
        let (info, _) = found.expect("session should exist");
        assert_eq!(info.client_name.as_deref(), Some("claude-code"));
        assert_eq!(info.client_version.as_deref(), Some("1.0.0"));

        drop(session);
        delete_session_data_with_conn(&conn, &id)?;
        Ok(())
    }

    #[test]
    fn test_broadcaster_noop() {
        let broadcaster = EventBroadcaster::noop();

        // Should not panic or error
        broadcaster.send(EventKind::Started);
        broadcaster.send(EventKind::ServerState {
            language: "rust".to_string(),
            state: "Ready".to_string(),
        });
    }

    #[test]
    fn test_active_languages_empty() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let session = create_session(&path, "/tmp/test-langs-empty")?;
        let id = session.info.id.clone();

        let langs = active_languages_with_conn(&conn, &id)?;
        assert!(langs.is_empty());

        drop(session);
        delete_session_data_with_conn(&conn, &id)?;
        Ok(())
    }

    #[test]
    fn test_active_languages_tracks_server_state() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let session = create_session(&path, "/tmp/test-langs-state")?;
        let id = session.info.id.clone();

        session.broadcast(EventKind::ServerState {
            language: "rust".to_string(),
            state: "Initializing".to_string(),
        });

        session.broadcast(EventKind::ServerState {
            language: "rust".to_string(),
            state: "Ready".to_string(),
        });

        let langs = active_languages_with_conn(&conn, &id)?;
        assert_eq!(langs, vec!["rust"]);

        drop(session);
        delete_session_data_with_conn(&conn, &id)?;
        Ok(())
    }

    #[test]
    fn test_active_languages_removes_dead() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let session = create_session(&path, "/tmp/test-langs-dead")?;
        let id = session.info.id.clone();

        session.broadcast(EventKind::ServerState {
            language: "rust".to_string(),
            state: "Ready".to_string(),
        });

        session.broadcast(EventKind::ServerState {
            language: "rust".to_string(),
            state: "Dead".to_string(),
        });

        let langs = active_languages_with_conn(&conn, &id)?;
        assert!(langs.is_empty());

        drop(session);
        delete_session_data_with_conn(&conn, &id)?;
        Ok(())
    }

    #[test]
    fn test_active_languages_multiple_languages() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let session = create_session(&path, "/tmp/test-langs-multi")?;
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

        let langs = active_languages_with_conn(&conn, &id)?;
        assert_eq!(langs, vec!["python", "rust", "typescript"]);

        drop(session);
        delete_session_data_with_conn(&conn, &id)?;
        Ok(())
    }

    /// Helper: create a dead session, optionally backdated.
    fn create_dead_session(
        db_path: &std::path::Path,
        conn: &Connection,
        workspace: &str,
        backdate_days: Option<i64>,
    ) -> Result<String> {
        let session = create_session(db_path, workspace)?;
        let id = session.info.id.clone();
        drop(session);

        if let Some(days) = backdate_days {
            let new_start = (Utc::now() - chrono::Duration::days(days)).to_rfc3339();
            conn.execute(
                "UPDATE sessions SET started_at = ?1 WHERE id = ?2",
                rusqlite::params![new_start, &id],
            )?;
        }
        Ok(id)
    }

    /// Single sequential test covering all `prune_sessions` behaviours.
    ///
    /// These must run in sequence because `prune_sessions` operates on the
    /// shared database and parallel execution causes interference.
    #[test]
    fn test_prune_sessions() -> Result<()> {
        let (_dir, path, conn) = test_db();
        // -- retention=-1 retains forever --
        let id_forever = create_dead_session(&path, &conn, "/tmp/test-prune-forever", Some(365))?;
        let removed = prune_sessions_with_conn(&conn, -1)?;
        assert_eq!(removed, 0, "retention=-1 should never prune");
        assert!(
            get_session_with_conn(&conn, &id_forever)?.is_some(),
            "session should still exist"
        );
        delete_session_data_with_conn(&conn, &id_forever)?;

        // -- retention=7 keeps recent, removes old --
        let id_recent = create_dead_session(&path, &conn, "/tmp/test-prune-recent", None)?;
        let id_old = create_dead_session(&path, &conn, "/tmp/test-prune-old", Some(10))?;

        let _ = prune_sessions_with_conn(&conn, 7)?;
        assert!(
            get_session_with_conn(&conn, &id_recent)?.is_some(),
            "recent dead session should survive prune"
        );
        assert!(
            get_session_with_conn(&conn, &id_old)?.is_none(),
            "old dead session should be pruned"
        );
        delete_session_data_with_conn(&conn, &id_recent)?;

        // -- retention=0 removes all dead --
        let id_zero = create_dead_session(&path, &conn, "/tmp/test-prune-zero", None)?;
        let _ = prune_sessions_with_conn(&conn, 0)?;
        assert!(
            get_session_with_conn(&conn, &id_zero)?.is_none(),
            "dead session should be removed with retention=0"
        );

        Ok(())
    }
}

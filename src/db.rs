// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! SQLite database connection management, schema creation, and migrations.
//!
//! Provides the foundation for all persistent state in Catenary. The database
//! file lives at `~/.local/state/catenary/catenary.db` (or platform equivalent).

use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Current schema version. Bump when adding migrations.
const SCHEMA_VERSION: u32 = 1;

/// Returns the path to the Catenary database file.
///
/// Uses the same directory resolution as [`crate::session::sessions_dir`]:
/// `dirs::state_dir()` with fallback to `dirs::data_local_dir()` or `/tmp`.
pub fn db_path() -> PathBuf {
    let state_dir = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    state_dir.join("catenary").join("catenary.db")
}

/// Opens a connection to the Catenary database with standard pragmas.
///
/// Sets WAL journal mode, 5-second busy timeout, and enables foreign keys.
/// Creates the parent directory if it does not exist.
///
/// # Errors
///
/// Returns an error if the parent directory cannot be created or the
/// database cannot be opened.
pub fn open() -> Result<Connection> {
    open_at(&db_path())
}

/// Opens a connection to a database at the given path with standard pragmas.
///
/// Like [`open`] but uses an explicit path instead of the default location.
/// Useful for testing with temporary directories.
///
/// # Errors
///
/// Returns an error if the parent directory cannot be created or the
/// database cannot be opened.
pub fn open_at(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("failed to create database directory: {}", parent.display())
        })?;
    }

    let conn = Connection::open(path)
        .with_context(|| format!("failed to open database: {}", path.display()))?;

    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA busy_timeout=5000;
         PRAGMA foreign_keys=ON;",
    )
    .context("failed to set database pragmas")?;

    Ok(conn)
}

/// Opens a connection and runs schema migrations if needed.
///
/// On a fresh database, creates all tables. On an existing database,
/// checks the schema version and applies any pending migrations.
///
/// # Errors
///
/// Returns an error if the connection cannot be opened, schema creation
/// fails, or a migration step fails.
pub fn open_and_migrate() -> Result<Connection> {
    open_and_migrate_at(&db_path())
}

/// Opens a connection at the given path and runs schema migrations if needed.
///
/// Like [`open_and_migrate`] but uses an explicit path instead of the default
/// location. Useful for testing with temporary directories.
///
/// # Errors
///
/// Returns an error if the connection cannot be opened, schema creation
/// fails, or a migration step fails.
pub fn open_and_migrate_at(path: &Path) -> Result<Connection> {
    let conn = open_at(path)?;

    if table_exists(&conn, "meta") {
        let version = current_schema_version(&conn)?;
        if version < SCHEMA_VERSION {
            // Future migrations would go here, applied sequentially:
            // if version < 2 { migrate_v1_to_v2(&conn)?; }
            // if version < 3 { migrate_v2_to_v3(&conn)?; }
            let _ = version; // no migrations yet beyond v1
        }
    } else {
        create_schema(&conn)?;
        migrate_legacy_data(&conn)?;
    }

    Ok(conn)
}

/// Checks whether a table exists in the database.
fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .is_ok_and(|count| count > 0)
}

/// Creates the full database schema in a single transaction.
///
/// # Errors
///
/// Returns an error if any CREATE TABLE or INSERT statement fails.
fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;

         CREATE TABLE IF NOT EXISTS meta (
             key   TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );
         INSERT OR IGNORE INTO meta (key, value) VALUES ('schema_version', '1');

         CREATE TABLE IF NOT EXISTS sessions (
             id             TEXT PRIMARY KEY,
             pid            INTEGER NOT NULL,
             display_name   TEXT NOT NULL,
             client_name    TEXT,
             client_version TEXT,
             started_at     TEXT NOT NULL,
             ended_at       TEXT,
             alive          INTEGER NOT NULL DEFAULT 1
         );

         CREATE TABLE IF NOT EXISTS workspace_roots (
             session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             root_path   TEXT NOT NULL,
             PRIMARY KEY (session_id, root_path)
         );

         CREATE TABLE IF NOT EXISTS events (
             id          INTEGER PRIMARY KEY AUTOINCREMENT,
             session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             timestamp   TEXT NOT NULL,
             kind        TEXT NOT NULL,
             payload     TEXT NOT NULL,
             created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         );

         CREATE INDEX IF NOT EXISTS idx_events_session_id ON events(session_id, id);
         CREATE INDEX IF NOT EXISTS idx_events_timestamp ON events(timestamp);

         CREATE TABLE IF NOT EXISTS language_servers (
             session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             name        TEXT NOT NULL,
             state       TEXT NOT NULL,
             PRIMARY KEY (session_id, name)
         );

         CREATE TABLE IF NOT EXISTS filter_history (
             id          INTEGER PRIMARY KEY AUTOINCREMENT,
             workspace   TEXT NOT NULL,
             pattern     TEXT NOT NULL,
             created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         );

         CREATE INDEX IF NOT EXISTS idx_filter_workspace ON filter_history(workspace, created_at DESC);

         CREATE TABLE IF NOT EXISTS root_sync_state (
             session_id  TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
             offset      INTEGER NOT NULL DEFAULT 0,
             roots       TEXT NOT NULL DEFAULT '[]'
         );

         COMMIT;",
    )
    .context("failed to create database schema")?;

    Ok(())
}

/// Reads the current schema version from the `meta` table.
///
/// # Errors
///
/// Returns an error if the `meta` table cannot be queried or the version
/// value cannot be parsed as a `u32`.
fn current_schema_version(conn: &Connection) -> Result<u32> {
    let version_str: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .context("failed to read schema_version from meta table")?;

    version_str
        .parse::<u32>()
        .with_context(|| format!("invalid schema_version: {version_str}"))
}

// ── Legacy filesystem migration ──────────────────────────────────────

/// Path to the legacy sessions directory (pre-SQLite migration).
fn legacy_sessions_dir() -> PathBuf {
    let state_dir = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    state_dir.join("catenary").join("sessions")
}

/// Path to the legacy locks directory (removed in lock removal phase).
fn legacy_locks_dir() -> PathBuf {
    let state_dir = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    state_dir.join("catenary").join("locks")
}

/// Migrates legacy filesystem session data to `SQLite`.
///
/// Called once when the database is first created. Detects legacy session
/// directories and imports their data. Also removes the obsolete locks
/// directory.
///
/// # Errors
///
/// Returns an error if the legacy sessions directory exists but cannot be
/// read, or if the migration fails for a systemic reason. Per-session
/// failures are logged and skipped.
fn migrate_legacy_data(conn: &Connection) -> Result<()> {
    migrate_legacy_from(conn, &legacy_sessions_dir(), &legacy_locks_dir())
}

/// Core legacy migration with explicit paths (enables testing with temp dirs).
fn migrate_legacy_from(conn: &Connection, sessions_dir: &Path, locks_dir: &Path) -> Result<()> {
    if !sessions_dir.exists() {
        return Ok(());
    }

    let sentinel = sessions_dir.join("migrated");
    if sentinel.exists() {
        return Ok(());
    }

    migrate_session_directories(conn, sessions_dir)?;

    if locks_dir.exists() {
        let _ = std::fs::remove_dir_all(locks_dir);
    }

    let _ = std::fs::write(&sentinel, "migrated to catenary.db");

    Ok(())
}

/// Walks the legacy sessions directory and migrates each session subdirectory.
fn migrate_session_directories(conn: &Connection, sessions_dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(sessions_dir)? {
        let entry = entry?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }

        if let Err(e) = migrate_one_session(conn, &dir) {
            tracing::warn!("failed to migrate legacy session {}: {e}", dir.display());
        }
    }
    Ok(())
}

/// Migrates a single legacy session directory to `SQLite`.
///
/// Parses `info.json` for session metadata and `events.jsonl` for event
/// history. Uses a savepoint so each session is migrated atomically.
fn migrate_one_session(conn: &Connection, dir: &Path) -> Result<()> {
    use crate::session::{SessionInfo, is_process_alive};

    let info_path = dir.join("info.json");
    let info: SessionInfo = serde_json::from_str(
        &std::fs::read_to_string(&info_path)
            .with_context(|| format!("failed to read {}", info_path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", info_path.display()))?;

    let dead_marker = dir.join("dead");
    let alive = !dead_marker.exists() && is_process_alive(info.pid);

    conn.execute_batch("SAVEPOINT legacy_session")?;

    let result = write_legacy_session(conn, dir, &info, alive);

    match result {
        Ok(()) => {
            conn.execute_batch("RELEASE SAVEPOINT legacy_session")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK TO SAVEPOINT legacy_session");
            Err(e)
        }
    }
}

/// Writes a single legacy session's data into the database.
fn write_legacy_session(
    conn: &Connection,
    dir: &Path,
    info: &crate::session::SessionInfo,
    alive: bool,
) -> Result<()> {
    use crate::session::{SessionEvent, event_kind_tag};

    conn.execute(
        "INSERT OR IGNORE INTO sessions \
         (id, pid, display_name, client_name, client_version, started_at, alive) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            &info.id,
            info.pid,
            &info.workspace,
            &info.client_name,
            &info.client_version,
            info.started_at.to_rfc3339(),
            i32::from(alive),
        ],
    )?;

    for root in info
        .workspace
        .split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        conn.execute(
            "INSERT OR IGNORE INTO workspace_roots (session_id, root_path) VALUES (?1, ?2)",
            rusqlite::params![&info.id, root],
        )?;
    }

    let events_path = dir.join("events.jsonl");
    if events_path.exists() {
        let file = std::fs::File::open(&events_path)?;
        let reader = std::io::BufReader::new(file);
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<SessionEvent>(&line) {
                let kind_tag = event_kind_tag(&event.kind);
                if let Ok(payload) = serde_json::to_string(&event.kind) {
                    conn.execute(
                        "INSERT INTO events (session_id, timestamp, kind, payload) \
                         VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![
                            &info.id,
                            event.timestamp.to_rfc3339(),
                            kind_tag,
                            payload,
                        ],
                    )?;
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_open_creates_db_file() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        let _conn = open_at(&path).expect("open_at failed");
        assert!(path.exists(), "database file should exist after open");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_open_and_migrate_creates_schema() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        let conn = open_and_migrate_at(&path).expect("open_and_migrate_at failed");

        let expected_tables = [
            "meta",
            "sessions",
            "workspace_roots",
            "events",
            "language_servers",
            "filter_history",
            "root_sync_state",
        ];

        for table in &expected_tables {
            assert!(
                table_exists(&conn, table),
                "table '{table}' should exist after migration"
            );
        }
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_open_and_migrate_idempotent() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        let conn1 = open_and_migrate_at(&path).expect("first open_and_migrate_at failed");
        drop(conn1);

        let conn2 = open_and_migrate_at(&path).expect("second open_and_migrate_at should succeed");

        assert!(
            table_exists(&conn2, "meta"),
            "meta table should still exist after second migration"
        );
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_schema_version() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        let conn = open_and_migrate_at(&path).expect("open_and_migrate_at failed");

        let version = current_schema_version(&conn).expect("failed to read schema version");
        assert_eq!(version, 1, "schema version should be 1");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_wal_mode() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        let conn = open_at(&path).expect("open_at failed");

        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("failed to query journal_mode");

        assert_eq!(mode, "wal", "journal mode should be WAL");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_foreign_keys_enabled() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        let conn = open_at(&path).expect("open_at failed");

        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("failed to query foreign_keys");

        assert_eq!(fk, 1, "foreign keys should be enabled");
    }

    // ── Legacy migration tests ──────────────────────────────────────

    /// Helper: create a fake legacy session directory with info.json and events.jsonl.
    #[allow(clippy::expect_used, reason = "test helper")]
    fn create_legacy_session(parent: &Path, id: &str, dead: bool) {
        let dir = parent.join(id);
        std::fs::create_dir_all(&dir).expect("create session dir");

        let info = serde_json::json!({
            "id": id,
            "pid": 99999,
            "workspace": "/tmp/test-workspace",
            "started_at": "2026-01-15T10:00:00Z"
        });
        std::fs::write(dir.join("info.json"), info.to_string()).expect("write info.json");

        let events = "\
            {\"timestamp\":\"2026-01-15T10:00:01Z\",\"type\":\"started\"}\n\
            {\"timestamp\":\"2026-01-15T10:00:02Z\",\"type\":\"tool_call\",\"tool\":\"grep\",\"file\":\"/tmp/test.rs\"}\n";
        std::fs::write(dir.join("events.jsonl"), events).expect("write events.jsonl");

        if dead {
            std::fs::write(dir.join("dead"), "").expect("write dead marker");
        }
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_migrate_legacy_sessions() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let db_path = dir.path().join("test.db");
        let conn = open_and_migrate_at(&db_path).expect("open db");

        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("create sessions dir");
        create_legacy_session(&sessions_dir, "legacy-001", true);

        let locks_dir = dir.path().join("locks");
        migrate_legacy_from(&conn, &sessions_dir, &locks_dir).expect("migration should succeed");

        // Verify session was inserted
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE id = 'legacy-001'",
                [],
                |row| row.get(0),
            )
            .expect("query sessions");
        assert_eq!(count, 1, "session should be migrated");

        // Verify alive=0 (dead marker present)
        let alive: bool = conn
            .query_row(
                "SELECT alive FROM sessions WHERE id = 'legacy-001'",
                [],
                |row| row.get(0),
            )
            .expect("query alive");
        assert!(!alive, "session with dead marker should not be alive");

        // Verify workspace root
        let root_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM workspace_roots WHERE session_id = 'legacy-001'",
                [],
                |row| row.get(0),
            )
            .expect("query roots");
        assert_eq!(root_count, 1, "workspace root should be migrated");

        // Verify events (started + tool_call = 2)
        let event_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE session_id = 'legacy-001'",
                [],
                |row| row.get(0),
            )
            .expect("query events");
        assert_eq!(event_count, 2, "both events should be migrated");

        // Verify sentinel
        assert!(
            sessions_dir.join("migrated").exists(),
            "sentinel file should be written"
        );
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_migrate_legacy_no_sessions_dir() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let db_path = dir.path().join("test.db");
        let conn = open_and_migrate_at(&db_path).expect("open db");

        let sessions_dir = dir.path().join("nonexistent");
        let locks_dir = dir.path().join("locks");
        migrate_legacy_from(&conn, &sessions_dir, &locks_dir)
            .expect("migration should be a no-op when no legacy dir exists");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_migrate_legacy_idempotent() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let db_path = dir.path().join("test.db");
        let conn = open_and_migrate_at(&db_path).expect("open db");

        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("create sessions dir");
        create_legacy_session(&sessions_dir, "legacy-idem", true);

        let locks_dir = dir.path().join("locks");
        migrate_legacy_from(&conn, &sessions_dir, &locks_dir)
            .expect("first migration should succeed");

        // Second migration should detect sentinel and skip
        migrate_legacy_from(&conn, &sessions_dir, &locks_dir)
            .expect("second migration should succeed (no-op)");

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE id = 'legacy-idem'",
                [],
                |row| row.get(0),
            )
            .expect("query sessions");
        assert_eq!(count, 1, "session should only appear once");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_migrate_legacy_deletes_locks() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let db_path = dir.path().join("test.db");
        let conn = open_and_migrate_at(&db_path).expect("open db");

        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("create sessions dir");
        create_legacy_session(&sessions_dir, "legacy-locks", true);

        let locks_dir = dir.path().join("locks");
        std::fs::create_dir_all(&locks_dir).expect("create locks dir");
        std::fs::write(locks_dir.join("some-lock"), "lock data").expect("write lock file");

        migrate_legacy_from(&conn, &sessions_dir, &locks_dir).expect("migration should succeed");

        assert!(
            !locks_dir.exists(),
            "locks directory should be deleted after migration"
        );
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_migrate_legacy_corrupt_session() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let db_path = dir.path().join("test.db");
        let conn = open_and_migrate_at(&db_path).expect("open db");

        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("create sessions dir");

        // Create a corrupt session (invalid info.json)
        let corrupt_dir = sessions_dir.join("corrupt-001");
        std::fs::create_dir_all(&corrupt_dir).expect("create corrupt dir");
        std::fs::write(corrupt_dir.join("info.json"), "not valid json")
            .expect("write corrupt info");

        // Create a valid session
        create_legacy_session(&sessions_dir, "valid-001", true);

        let locks_dir = dir.path().join("locks");
        migrate_legacy_from(&conn, &sessions_dir, &locks_dir)
            .expect("migration should succeed despite corrupt session");

        // Valid session should be migrated
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE id = 'valid-001'",
                [],
                |row| row.get(0),
            )
            .expect("query valid session");
        assert_eq!(count, 1, "valid session should be migrated");

        // Corrupt session should not be in the database
        let corrupt_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE id = 'corrupt-001'",
                [],
                |row| row.get(0),
            )
            .expect("query corrupt session");
        assert_eq!(corrupt_count, 0, "corrupt session should not be migrated");
    }
}

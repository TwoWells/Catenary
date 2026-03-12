// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Point-in-time file recovery from snapshots.
//!
//! The replace tool creates a snapshot before every batch edit. This module
//! provides the `catenary restore` CLI: listing snapshots, restoring by ID
//! or most recent, and creating sidecars for pre-restore state.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use rusqlite::Connection;

/// Lists snapshots, optionally filtered to a single file.
///
/// Returns a formatted string grouped by file path with newest snapshots first.
/// If no snapshots exist, returns an appropriate message.
///
/// # Errors
///
/// Returns an error if the database query fails.
pub fn list_snapshots(conn: &Connection, file: Option<&str>) -> Result<String> {
    let mut output = String::new();

    if let Some(file) = file {
        let mut stmt = conn.prepare(
            "SELECT id, file_path, source, pattern, count, created_at \
             FROM snapshots WHERE file_path = ?1 ORDER BY id DESC",
        )?;

        let rows: Vec<SnapshotRow> = stmt
            .query_map([file], |row| {
                Ok(SnapshotRow {
                    id: row.get(0)?,
                    file_path: row.get(1)?,
                    source: row.get(2)?,
                    pattern: row.get(3)?,
                    count: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        if rows.is_empty() {
            return Ok(format!("no snapshots for {file}"));
        }

        output.push_str(&rows[0].file_path);
        output.push('\n');
        for row in &rows {
            output.push_str(&format_snapshot_line(row));
            output.push('\n');
        }
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, file_path, source, pattern, count, created_at \
             FROM snapshots ORDER BY file_path, id DESC",
        )?;

        let rows: Vec<SnapshotRow> = stmt
            .query_map([], |row| {
                Ok(SnapshotRow {
                    id: row.get(0)?,
                    file_path: row.get(1)?,
                    source: row.get(2)?,
                    pattern: row.get(3)?,
                    count: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        if rows.is_empty() {
            return Ok("no snapshots".to_owned());
        }

        let mut current_file: Option<&str> = None;
        for row in &rows {
            if current_file != Some(&row.file_path) {
                if current_file.is_some() {
                    // Blank line between file groups.
                    output.push('\n');
                }
                output.push_str(&row.file_path);
                output.push('\n');
                current_file = Some(&row.file_path);
            }
            output.push_str(&format_snapshot_line(row));
            output.push('\n');
        }
    }

    // Remove trailing newline.
    if output.ends_with('\n') {
        output.pop();
    }

    Ok(output)
}

/// Restores a file to the content of a specific snapshot.
///
/// If the file currently exists on disk, a sidecar copy and a restore snapshot
/// are created first. If the file does not exist, it is created directly.
///
/// # Errors
///
/// Returns an error if the snapshot ID does not exist or file I/O fails.
pub fn restore_by_id(conn: &Connection, id: i64) -> Result<String> {
    let (file_path, content, created_at): (String, Vec<u8>, String) = conn
        .query_row(
            "SELECT file_path, content, created_at FROM snapshots WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|_| anyhow!("no snapshot with id {id}"))?;

    let target_path = Path::new(&file_path);
    let time = extract_time(&created_at);

    if target_path.exists() {
        let current_content = std::fs::read(target_path)
            .map_err(|e| anyhow!("failed to read {}: {e}", target_path.display()))?;

        // Create restore snapshot and sidecar.
        let (sidecar, restore_id) = create_restore_sidecar(conn, target_path, &current_content)?;

        std::fs::rename(target_path, &sidecar)
            .map_err(|e| anyhow!("failed to move {} to sidecar: {e}", target_path.display()))?;
        std::fs::write(target_path, &content)
            .map_err(|e| anyhow!("failed to write {}: {e}", target_path.display()))?;

        Ok(format!(
            "restored {file_path} to snapshot #{id} ({time})\n\
             pre-restore state saved to {} [snapshot #{restore_id}]",
            sidecar.display(),
        ))
    } else {
        // Ensure parent directory exists.
        if let Some(parent) = target_path.parent().filter(|p| !p.exists()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow!("failed to create directory {}: {e}", parent.display()))?;
        }

        std::fs::write(target_path, &content)
            .map_err(|e| anyhow!("failed to write {}: {e}", target_path.display()))?;

        Ok(format!(
            "restored {file_path} to snapshot #{id} ({time})\n\
             (file was missing — no sidecar created)",
        ))
    }
}

/// Restores a file to its most recent snapshot.
///
/// # Errors
///
/// Returns an error if no snapshots exist for the file or the restore fails.
pub fn restore_most_recent(conn: &Connection, file: &str) -> Result<String> {
    let id: i64 = conn
        .query_row(
            "SELECT id FROM snapshots WHERE file_path = ?1 ORDER BY id DESC LIMIT 1",
            [file],
            |row| row.get(0),
        )
        .map_err(|_| anyhow!("no snapshots for {file}"))?;

    restore_by_id(conn, id)
}

// ─── Helpers ────────────────────────────────────────────────────────────

/// A row from the snapshots table used for listing.
struct SnapshotRow {
    id: i64,
    file_path: String,
    source: String,
    pattern: Option<String>,
    count: Option<i64>,
    created_at: String,
}

/// Formats a single snapshot line for display.
///
/// ```text
///   #5  14:40  restore
///   #3  14:32  replace  4 edits  (8 replacements)
/// ```
fn format_snapshot_line(row: &SnapshotRow) -> String {
    let time = extract_time(&row.created_at);

    match row.source.as_str() {
        "restore" => format!("  #{id}  {time}  restore", id = row.id),
        "replace" => {
            let pattern = row.pattern.as_deref().unwrap_or("?");
            let count = row.count.unwrap_or(0);
            let noun = if count == 1 {
                "replacement"
            } else {
                "replacements"
            };
            format!(
                "  #{id}  {time}  replace  {pattern}  ({count} {noun})",
                id = row.id,
            )
        }
        other => format!("  #{id}  {time}  {other}", id = row.id),
    }
}

/// Extracts HH:MM from an ISO 8601 timestamp.
///
/// Falls back to the raw string if the format is unexpected.
fn extract_time(timestamp: &str) -> &str {
    // ISO 8601: "2026-03-07T14:32:00" → "14:32"
    if let Some(t_pos) = timestamp.find('T') {
        let after_t = &timestamp[t_pos + 1..];
        if after_t.len() >= 5 && after_t.as_bytes()[2] == b':' {
            return &after_t[..5];
        }
    }
    // datetime('now') format: "2026-03-07 14:32:00" → "14:32"
    if timestamp.len() >= 16 && timestamp.as_bytes()[10] == b' ' {
        let after_space = &timestamp[11..];
        if after_space.len() >= 5 && after_space.as_bytes()[2] == b':' {
            return &after_space[..5];
        }
    }
    timestamp
}

/// Computes the sidecar path for a file.
///
/// With extension: `handler.catenary_snapshot_5.rs`
/// Without extension: `Makefile.catenary_snapshot_5`
#[must_use]
pub fn sidecar_path(file_path: &Path, snapshot_id: i64) -> PathBuf {
    let stem = file_path.file_stem().unwrap_or_default().to_string_lossy();
    let tag = format!("{stem}.catenary_snapshot_{snapshot_id}");

    if let Some(ext) = file_path.extension() {
        let name = format!("{tag}.{}", ext.to_string_lossy());
        file_path.with_file_name(name)
    } else {
        file_path.with_file_name(tag)
    }
}

/// Creates a restore snapshot row and returns the sidecar path and snapshot ID.
///
/// Handles sidecar collisions by deleting the snapshot row and retrying with
/// the next autoincrement ID until a free sidecar path is found.
fn create_restore_sidecar(
    conn: &Connection,
    file_path: &Path,
    content: &[u8],
) -> Result<(PathBuf, i64)> {
    let file_str = file_path.to_string_lossy();

    loop {
        conn.execute(
            "INSERT INTO snapshots \
                 (file_path, content, source, pattern, replacement, count, created_at, session_id) \
             VALUES (?1, ?2, 'restore', NULL, NULL, NULL, datetime('now'), NULL)",
            rusqlite::params![file_str.as_ref(), content],
        )
        .map_err(|e| anyhow!("failed to insert restore snapshot: {e}"))?;

        let restore_id = conn.last_insert_rowid();
        let sidecar = sidecar_path(file_path, restore_id);

        if !sidecar.exists() {
            return Ok((sidecar, restore_id));
        }

        // Sidecar collision — delete this row and retry.
        conn.execute("DELETE FROM snapshots WHERE id = ?1", [restore_id])
            .map_err(|e| anyhow!("failed to delete colliding snapshot: {e}"))?;
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crate::db::open_and_migrate_at;

    fn open_test_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = open_and_migrate_at(&dir.path().join("test.db")).expect("open db");
        (dir, conn)
    }

    /// Inserts a replace-style snapshot and returns its row ID.
    fn insert_replace_snapshot(
        conn: &Connection,
        file_path: &str,
        content: &[u8],
        edit_count: usize,
        replacement_count: i64,
    ) -> i64 {
        let pattern = format!("{edit_count} edits");
        conn.execute(
            "INSERT INTO snapshots \
                 (file_path, content, source, pattern, replacement, count, created_at, session_id) \
             VALUES (?1, ?2, 'replace', ?3, NULL, ?4, datetime('now'), NULL)",
            rusqlite::params![file_path, content, pattern, replacement_count],
        )
        .expect("insert snapshot");
        conn.last_insert_rowid()
    }

    // ─── Restore tests ─────────────────────────────────────────────────

    #[test]
    fn test_restore_most_recent() {
        let (_db_dir, conn) = open_test_db();
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("test.rs");
        let file_str = file.to_string_lossy().to_string();

        // Original content.
        let original = b"fn main() {}";
        std::fs::write(&file, original).expect("write original");

        // Create snapshot with the original content.
        insert_replace_snapshot(&conn, &file_str, original, 1, 1);

        // Modify the file (simulate post-replace state).
        let modified = b"fn main() { changed }";
        std::fs::write(&file, modified).expect("write modified");

        // Restore.
        let msg = restore_most_recent(&conn, &file_str).expect("restore");
        assert!(msg.contains("restored"), "message: {msg}");

        // File should have snapshot content.
        let restored = std::fs::read(&file).expect("read restored");
        assert_eq!(restored, original);

        // Sidecar should exist with pre-restore content.
        let sidecars: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("catenary_snapshot")
            })
            .collect();
        assert_eq!(sidecars.len(), 1, "expected one sidecar");
        let sidecar_content = std::fs::read(sidecars[0].path()).expect("read sidecar");
        assert_eq!(sidecar_content, modified);
    }

    #[test]
    fn test_restore_by_id() {
        let (_db_dir, conn) = open_test_db();
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("test.rs");
        let file_str = file.to_string_lossy().to_string();

        let content_v1 = b"version 1";
        let content_v2 = b"version 2";
        let content_v3 = b"version 3";

        std::fs::write(&file, content_v1).expect("write");
        let id1 = insert_replace_snapshot(&conn, &file_str, content_v1, 1, 1);

        std::fs::write(&file, content_v2).expect("write");
        insert_replace_snapshot(&conn, &file_str, content_v2, 1, 1);

        std::fs::write(&file, content_v3).expect("write");

        // Restore to the first snapshot specifically.
        let msg = restore_by_id(&conn, id1).expect("restore");
        assert!(msg.contains(&format!("#{id1}")), "message: {msg}");

        let restored = std::fs::read(&file).expect("read");
        assert_eq!(restored, content_v1);
    }

    #[test]
    fn test_sidecar_naming() {
        let path = Path::new("/tmp/handler.rs");
        let sidecar = sidecar_path(path, 5);
        assert_eq!(
            sidecar,
            PathBuf::from("/tmp/handler.catenary_snapshot_5.rs")
        );
    }

    #[test]
    fn test_sidecar_naming_extensionless() {
        let path = Path::new("/tmp/Makefile");
        let sidecar = sidecar_path(path, 7);
        assert_eq!(sidecar, PathBuf::from("/tmp/Makefile.catenary_snapshot_7"));
    }

    #[test]
    fn test_sidecar_content() {
        let (_db_dir, conn) = open_test_db();
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("test.rs");
        let file_str = file.to_string_lossy().to_string();

        let snapshot_content = b"snapshot content";
        std::fs::write(&file, snapshot_content).expect("write");
        insert_replace_snapshot(&conn, &file_str, snapshot_content, 1, 1);

        let current_content = b"current content";
        std::fs::write(&file, current_content).expect("write");

        restore_most_recent(&conn, &file_str).expect("restore");

        // Find the sidecar.
        let sidecars: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("catenary_snapshot")
            })
            .collect();
        assert_eq!(sidecars.len(), 1);
        let sidecar_content = std::fs::read(sidecars[0].path()).expect("read sidecar");
        assert_eq!(sidecar_content, current_content);
    }

    #[test]
    fn test_sidecar_collision() {
        let (_db_dir, conn) = open_test_db();
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("test.rs");
        let file_str = file.to_string_lossy().to_string();

        let original = b"original";
        std::fs::write(&file, original).expect("write");
        insert_replace_snapshot(&conn, &file_str, original, 1, 1);

        let current = b"current";
        std::fs::write(&file, current).expect("write");

        // Pre-create sidecar at the expected path to force a collision.
        // The restore snapshot will get the next ID after the replace snapshot.
        // We need to predict what ID the restore snapshot will get and block it.
        let next_id = conn
            .query_row("SELECT MAX(id) + 1 FROM snapshots", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("max id");
        let blocked_sidecar = sidecar_path(&file, next_id);
        std::fs::write(&blocked_sidecar, b"blocker").expect("write blocker");

        let msg = restore_most_recent(&conn, &file_str).expect("restore");
        assert!(msg.contains("restored"), "message: {msg}");

        // The sidecar used should NOT be the blocked one — it should have a higher ID.
        let restored = std::fs::read(&file).expect("read");
        assert_eq!(restored, original);

        // There should be two catenary_snapshot files: the blocker and the actual sidecar.
        let sidecars: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("catenary_snapshot")
            })
            .collect();
        assert!(
            sidecars.len() >= 2,
            "expected at least 2 sidecar files, got {}",
            sidecars.len()
        );
    }

    #[test]
    fn test_restore_chain() {
        let (_db_dir, conn) = open_test_db();
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("test.rs");
        let file_str = file.to_string_lossy().to_string();

        let content_v1 = b"version 1";
        let content_v2 = b"version 2";

        std::fs::write(&file, content_v1).expect("write");
        let id1 = insert_replace_snapshot(&conn, &file_str, content_v1, 1, 1);

        std::fs::write(&file, content_v2).expect("write");
        insert_replace_snapshot(&conn, &file_str, content_v2, 1, 1);

        let current = b"current state";
        std::fs::write(&file, current).expect("write");

        // Restore to first snapshot.
        restore_by_id(&conn, id1).expect("restore");

        // File should have v1 content.
        let restored = std::fs::read(&file).expect("read");
        assert_eq!(restored, content_v1);

        // Sidecar should have pre-restore content ("current state").
        let sidecars: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("catenary_snapshot")
            })
            .collect();
        assert_eq!(sidecars.len(), 1);
        let sidecar_content = std::fs::read(sidecars[0].path()).expect("read sidecar");
        assert_eq!(sidecar_content, current);
    }

    #[test]
    fn test_restore_deleted_file() {
        let (_db_dir, conn) = open_test_db();
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("test.rs");
        let file_str = file.to_string_lossy().to_string();

        let content = b"original content";
        std::fs::write(&file, content).expect("write");
        insert_replace_snapshot(&conn, &file_str, content, 1, 1);

        // Delete the file.
        std::fs::remove_file(&file).expect("remove");
        assert!(!file.exists());

        let msg = restore_most_recent(&conn, &file_str).expect("restore");
        assert!(msg.contains("file was missing"), "message: {msg}");

        // File should be recreated.
        let restored = std::fs::read(&file).expect("read");
        assert_eq!(restored, content);

        // No sidecar should exist.
        let sidecars: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("catenary_snapshot")
            })
            .collect();
        assert_eq!(sidecars.len(), 0);
    }

    // ─── List tests ─────────────────────────────────────────────────────

    #[test]
    fn test_list_all() {
        let (_db_dir, conn) = open_test_db();

        // Insert snapshots for two files.
        conn.execute(
            "INSERT INTO snapshots (file_path, content, source, pattern, count, created_at) \
             VALUES ('src/a.rs', X'00', 'replace', '2 edits', 5, '2026-03-07T14:30:00')",
            [],
        )
        .expect("insert");
        conn.execute(
            "INSERT INTO snapshots (file_path, content, source, pattern, count, created_at) \
             VALUES ('src/a.rs', X'00', 'replace', '1 edits', 3, '2026-03-07T14:32:00')",
            [],
        )
        .expect("insert");
        conn.execute(
            "INSERT INTO snapshots (file_path, content, source, pattern, count, created_at) \
             VALUES ('src/b.rs', X'00', 'replace', '3 edits', 10, '2026-03-07T14:35:00')",
            [],
        )
        .expect("insert");

        let output = list_snapshots(&conn, None).expect("list");

        // Should be grouped by file.
        assert!(output.contains("src/a.rs"), "output: {output}");
        assert!(output.contains("src/b.rs"), "output: {output}");

        // Newest first within each group — a.rs #2 before #1.
        let lines: Vec<&str> = output.lines().collect();
        let a_header = lines
            .iter()
            .position(|l| *l == "src/a.rs")
            .expect("a.rs header");
        let a_line1 = lines[a_header + 1];
        let a_line2 = lines[a_header + 2];
        assert!(a_line1.contains("#2"), "first a.rs line: {a_line1}");
        assert!(a_line2.contains("#1"), "second a.rs line: {a_line2}");
    }

    #[test]
    fn test_list_single_file() {
        let (_db_dir, conn) = open_test_db();

        conn.execute(
            "INSERT INTO snapshots (file_path, content, source, pattern, count, created_at) \
             VALUES ('src/a.rs', X'00', 'replace', '1 edits', 1, '2026-03-07T14:30:00')",
            [],
        )
        .expect("insert");
        conn.execute(
            "INSERT INTO snapshots (file_path, content, source, pattern, count, created_at) \
             VALUES ('src/b.rs', X'00', 'replace', '1 edits', 1, '2026-03-07T14:35:00')",
            [],
        )
        .expect("insert");

        let output = list_snapshots(&conn, Some("src/a.rs")).expect("list");
        assert!(output.contains("src/a.rs"), "output: {output}");
        assert!(!output.contains("src/b.rs"), "output: {output}");
    }

    #[test]
    fn test_list_empty() {
        let (_db_dir, conn) = open_test_db();
        let output = list_snapshots(&conn, None).expect("list");
        assert_eq!(output, "no snapshots");
    }

    #[test]
    fn test_list_restore_source() {
        let (_db_dir, conn) = open_test_db();

        conn.execute(
            "INSERT INTO snapshots (file_path, content, source, pattern, count, created_at) \
             VALUES ('src/a.rs', X'00', 'restore', NULL, NULL, '2026-03-07T14:40:00')",
            [],
        )
        .expect("insert");

        let output = list_snapshots(&conn, None).expect("list");
        assert!(output.contains("restore"), "output: {output}");
        // Should not contain pattern/count noise.
        assert!(!output.contains("edits"), "output: {output}");
        assert!(!output.contains("replacements"), "output: {output}");
    }

    // ─── Error tests ────────────────────────────────────────────────────

    #[test]
    fn test_no_snapshots() {
        let (_db_dir, conn) = open_test_db();
        let result = restore_most_recent(&conn, "nonexistent.rs");
        assert!(result.is_err());
        let err = result.expect_err("expected error").to_string();
        assert!(
            err.contains("no snapshots for nonexistent.rs"),
            "error: {err}"
        );
    }

    #[test]
    fn test_invalid_id() {
        let (_db_dir, conn) = open_test_db();
        let result = restore_by_id(&conn, 999);
        assert!(result.is_err());
        let err = result.expect_err("expected error").to_string();
        assert!(err.contains("no snapshot with id 999"), "error: {err}");
    }
}

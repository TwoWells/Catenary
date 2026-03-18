// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! CLI subcommands: list, monitor, status, query, and gc.

#![allow(clippy::print_stdout, reason = "CLI tool needs to output to stdout")]
#![allow(clippy::print_stderr, reason = "CLI tool needs to output to stderr")]

use anyhow::Result;
use chrono::{Local, Utc};
use regex::Regex;
use std::path::Path;
use std::time::Duration;

use crate::cli::{self, ColorConfig, ColumnWidths, QueryFormat};
use crate::session::{self, Direction, EventKind, Protocol, SessionEvent};

/// List all active sessions.
///
/// # Errors
///
/// Returns an error if listing sessions fails.
pub fn run_list() -> Result<()> {
    let conn = crate::db::open_and_migrate()?;
    let sessions = session::list_sessions_with_conn(&conn)?;

    if sessions.is_empty() {
        println!("No active Catenary sessions");
        return Ok(());
    }

    let term_width = cli::terminal_width();
    let widths = ColumnWidths::calculate(term_width);
    let colors = cli::ColorConfig::new(false);

    // Print header
    println!(
        "{:>width_num$} {:<width_id$} {:<width_pid$} {:<width_client$} {:<width_ws$} STARTED",
        "#",
        "ID",
        "PID",
        "CLIENT",
        "WORKSPACE",
        width_num = widths.row_num,
        width_id = widths.id,
        width_pid = widths.pid,
        width_client = widths.client,
        width_ws = widths.workspace,
    );
    println!("{}", "-".repeat(term_width.min(120)));

    // Indent for the second line (aligns with ID column)
    let indent = " ".repeat(widths.row_num + 1);

    for (idx, (s, alive)) in sessions.iter().enumerate() {
        let client = match (&s.client_name, &s.client_version) {
            (Some(name), Some(ver)) => format!("{name} v{ver}"),
            (Some(name), None) => name.clone(),
            _ => "-".to_string(),
        };

        let ago = format_duration_ago(s.started_at);

        // Truncate fields to fit column widths
        let display_id = s.client_session_id.as_deref().unwrap_or(&s.id);
        let id = cli::truncate(display_id, widths.id);
        let workspace = cli::truncate(&s.workspace, widths.workspace);
        let client = cli::truncate(&client, widths.client);

        let row_str = format!(
            "{:>width_num$} {:<width_id$} {:<width_pid$} {:<width_client$} {:<width_ws$} {}",
            idx + 1,
            id,
            s.pid,
            client,
            workspace,
            ago,
            width_num = widths.row_num,
            width_id = widths.id,
            width_pid = widths.pid,
            width_client = widths.client,
            width_ws = widths.workspace,
        );

        if *alive {
            println!("{row_str}");
            // Get active languages for this session (shown on second line)
            let languages = session::active_languages_with_conn(&conn, &s.id).unwrap_or_default();
            if !languages.is_empty() {
                let lang_str = languages.join(", ");
                println!(
                    "{}",
                    colors.dim(&format!("{indent}language servers: {lang_str}"))
                );
            }
        } else {
            println!("{} (dead)", colors.dim(&row_str));
        }
    }

    Ok(())
}

/// Resolve a session ID from either a row number or ID prefix.
///
/// # Errors
///
/// Returns an error if the session cannot be found.
pub fn resolve_session_id(conn: &rusqlite::Connection, id: &str) -> Result<session::SessionInfo> {
    // Try parsing as a row number first (1-indexed)
    if let Ok(row_num) = id.parse::<usize>()
        && row_num > 0
    {
        let sessions = session::list_sessions_with_conn(conn)?;
        if let Some((s, _)) = sessions.get(row_num - 1) {
            return Ok(s.clone());
        }
        // Row number out of range — try as session ID prefix before giving up.
        // Session IDs are hex strings that may be all digits (e.g., "025586387"),
        // so a purely numeric input could be either a row number or a session ID.
        if let Ok(session) = find_session(conn, id) {
            return Ok(session);
        }
        anyhow::bail!("Row number {} out of range (1-{})", row_num, sessions.len());
    }

    // Fall back to find_session (ID prefix matching)
    find_session(conn, id)
}

/// Monitor events from a session.
///
/// # Errors
///
/// Returns an error if the session cannot be found or monitoring fails.
pub fn run_monitor(id: &str, raw: bool, nocolor: bool, filter: Option<&str>) -> Result<()> {
    let conn = crate::db::open_and_migrate()?;
    // Resolve session ID (supports row numbers and prefix matching)
    let session = resolve_session_id(&conn, id)?;
    let full_id = session.id;

    let colors = ColorConfig::new(nocolor);
    let term_width = cli::terminal_width();

    // Compile filter regex if provided
    let filter_regex = filter
        .as_ref()
        .map(|f| Regex::new(f))
        .transpose()
        .map_err(|e| anyhow::anyhow!("Invalid filter regex: {e}"))?;

    println!("Monitoring session {full_id} (Ctrl+C to stop)\n");

    let mut reader = session::tail_events(&full_id)?;

    // Track last progress (language, title) for line collapsing.
    // When consecutive progress events share the same title, the monitor
    // overwrites the previous line instead of scrolling.
    let mut last_progress: Option<(String, String)> = None;

    loop {
        match reader.try_next_event() {
            Ok(Some(event)) => {
                // Apply filter if set
                if let Some(ref re) = filter_regex {
                    let event_str = format!("{:?}", event.kind);
                    if !re.is_match(&event_str) {
                        continue;
                    }
                }

                if raw {
                    print_event_raw(&event);
                } else {
                    // Collapse consecutive progress lines with the same title
                    if let EventKind::Progress {
                        ref language,
                        ref title,
                        ..
                    } = event.kind
                    {
                        let key = (language.clone(), title.clone());
                        if last_progress.as_ref() == Some(&key) {
                            // Same progress context — erase previous line
                            print!("\x1b[A\x1b[2K");
                        }
                        last_progress = Some(key);
                    } else {
                        last_progress = None;
                    }
                    print_event_annotated(&event, &colors, term_width);
                }
            }
            Ok(None) => {
                // No new event — check liveness
                std::thread::sleep(Duration::from_millis(100));

                if let Ok(Some((_, alive))) = session::get_session_with_conn(&conn, &full_id) {
                    if !alive {
                        println!("\nSession ended (process dead)");
                        break;
                    }
                } else {
                    println!("\nSession ended (files removed)");
                    break;
                }
            }
            Err(_) => {
                println!("\nSession ended");
                break;
            }
        }
    }

    Ok(())
}

/// Show status of a session.
///
/// # Errors
///
/// Returns an error if the session cannot be found.
pub fn run_status(id: &str) -> Result<()> {
    let conn = crate::db::open_and_migrate()?;
    let session = find_session(&conn, id)?;

    println!("Session: {}", session.id);
    println!("PID: {}", session.pid);
    println!("Workspace: {}", session.workspace);
    println!(
        "Started: {} ({})",
        session
            .started_at
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S"),
        format_duration_ago(session.started_at)
    );

    if let Some(name) = &session.client_name {
        print!("Client: {name}");
        if let Some(ver) = &session.client_version {
            print!(" v{ver}");
        }
        println!();
    }

    // Show recent events
    println!("\nRecent events:");
    let events = session::monitor_events_with_conn(&conn, &session.id)?;
    let recent: Vec<_> = events.iter().rev().take(10).collect();

    for event in recent.iter().rev() {
        print_event(event);
    }

    Ok(())
}

/// Parse a human-friendly duration string into a UTC cutoff timestamp.
///
/// Accepted formats:
/// - `Nm` — N minutes ago
/// - `Nh` — N hours ago
/// - `Nd` — N days ago
/// - `today` — midnight local time today
///
/// # Errors
///
/// Returns an error if the string is not in a recognised format.
pub(crate) fn parse_since(s: &str) -> Result<chrono::DateTime<Utc>> {
    if s == "today" {
        let today = Local::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| anyhow::anyhow!("failed to compute midnight"))?;
        let local_midnight = today
            .and_local_timezone(Local)
            .single()
            .ok_or_else(|| anyhow::anyhow!("ambiguous local midnight"))?;
        return Ok(local_midnight.with_timezone(&Utc));
    }

    let (digits, unit) = s
        .strip_suffix('m')
        .map(|d| (d, "m"))
        .or_else(|| s.strip_suffix('h').map(|d| (d, "h")))
        .or_else(|| s.strip_suffix('d').map(|d| (d, "d")))
        .ok_or_else(|| {
            anyhow::anyhow!("unrecognised duration: {s} (expected Nm, Nh, Nd, or today)")
        })?;

    let n: i64 = digits
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid number in duration: {s}"))?;

    let duration = match unit {
        "m" => chrono::Duration::minutes(n),
        "h" => chrono::Duration::hours(n),
        "d" => chrono::Duration::days(n),
        _ => unreachable!(),
    };

    Ok(Utc::now() - duration)
}

/// Query events from the database.
///
/// Supports structured filters (`--session`, `--since`, `--kind`, `--search`)
/// and raw SQL (`--sql`). Results are printed in the chosen format.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or the query fails.
#[allow(
    clippy::too_many_lines,
    reason = "Sequential query building and output formatting"
)]
pub fn run_query(
    conn: &rusqlite::Connection,
    session_filter: Option<&str>,
    since: Option<&str>,
    kind: Option<&str>,
    search: Option<&str>,
    raw_sql: Option<&str>,
    format: QueryFormat,
) -> Result<()> {
    if let Some(sql) = raw_sql {
        let mut stmt = conn.prepare(sql)?;
        let col_count = stmt.column_count();
        let col_names: Vec<String> = (0..col_count)
            .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
            .collect();

        let mut rows_out: Vec<Vec<String>> = Vec::new();
        let mut db_rows = stmt.query([])?;
        while let Some(row) = db_rows.next()? {
            let mut vals = Vec::with_capacity(col_count);
            for i in 0..col_count {
                let val: String = row
                    .get::<_, rusqlite::types::Value>(i)
                    .map(|v| format_sql_value(&v))
                    .unwrap_or_default();
                vals.push(val);
            }
            rows_out.push(vals);
        }
        drop(db_rows);

        print_query_results(&col_names, &rows_out, format);
        return Ok(());
    }

    // Build structured query
    let mut conditions: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(sid) = session_filter {
        let resolved = resolve_session_id(conn, sid)?;
        conditions.push(format!("e.session_id = ?{}", params.len() + 1));
        params.push(Box::new(resolved.id));
    }

    if let Some(since_str) = since {
        let cutoff = parse_since(since_str)?;
        conditions.push(format!("e.timestamp > ?{}", params.len() + 1));
        params.push(Box::new(cutoff.to_rfc3339()));
    }

    if let Some(k) = kind {
        conditions.push(format!("e.kind = ?{}", params.len() + 1));
        params.push(Box::new(k.to_string()));
    }

    if let Some(s) = search {
        conditions.push(format!("e.payload LIKE ?{}", params.len() + 1));
        params.push(Box::new(format!("%{s}%")));
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };

    let sql = format!(
        "SELECT e.id, e.session_id, e.timestamp, e.kind, e.payload \
         FROM events e{where_clause} ORDER BY e.id DESC LIMIT 100"
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let mut stmt = conn.prepare(&sql)?;
    let mut db_rows = stmt.query(param_refs.as_slice())?;

    let col_names = vec![
        "ID".to_string(),
        "SESSION".to_string(),
        "TIME".to_string(),
        "KIND".to_string(),
        "PAYLOAD".to_string(),
    ];
    let mut rows_out: Vec<Vec<String>> = Vec::new();
    while let Some(row) = db_rows.next()? {
        let id: i64 = row.get(0)?;
        let sid: String = row.get(1)?;
        let ts: String = row.get(2)?;
        let k: String = row.get(3)?;
        let payload: String = row.get(4)?;

        // Shorten session ID and timestamp for table display
        let short_sid = if sid.len() > 8 { &sid[..8] } else { &sid };
        let short_ts = chrono::DateTime::parse_from_rfc3339(&ts)
            .map(|dt| dt.with_timezone(&Local).format("%H:%M:%S").to_string())
            .unwrap_or(ts);

        rows_out.push(vec![
            id.to_string(),
            short_sid.to_string(),
            short_ts,
            k,
            payload,
        ]);
    }

    print_query_results(&col_names, &rows_out, format);
    Ok(())
}

/// Format a `rusqlite` value as a display string.
fn format_sql_value(val: &rusqlite::types::Value) -> String {
    match val {
        rusqlite::types::Value::Null => "NULL".to_string(),
        rusqlite::types::Value::Integer(i) => i.to_string(),
        rusqlite::types::Value::Real(f) => f.to_string(),
        rusqlite::types::Value::Text(s) => s.clone(),
        rusqlite::types::Value::Blob(b) => format!("<blob {} bytes>", b.len()),
    }
}

/// Print query results in the chosen format.
fn print_query_results(col_names: &[String], rows: &[Vec<String>], format: QueryFormat) {
    if rows.is_empty() {
        println!("No results");
        return;
    }

    match format {
        QueryFormat::Table => {
            // Calculate column widths
            let mut widths: Vec<usize> = col_names.iter().map(String::len).collect();
            for row in rows {
                for (i, val) in row.iter().enumerate() {
                    if i < widths.len() {
                        widths[i] = widths[i].max(val.len());
                    }
                }
            }

            // Cap payload column at 80 chars
            if let Some(last) = widths.last_mut() {
                *last = (*last).min(80);
            }

            // Header
            let header: Vec<String> = col_names
                .iter()
                .zip(&widths)
                .map(|(name, w)| format!("{name:<w$}"))
                .collect();
            println!("{}", header.join("  "));
            println!(
                "{}",
                widths
                    .iter()
                    .map(|w| "-".repeat(*w))
                    .collect::<Vec<_>>()
                    .join("  ")
            );

            // Rows
            for row in rows {
                let formatted: Vec<String> = row
                    .iter()
                    .zip(&widths)
                    .map(|(val, w)| {
                        if val.len() > *w {
                            format!("{}...", &val[..w.saturating_sub(3)])
                        } else {
                            format!("{val:<w$}")
                        }
                    })
                    .collect();
                println!("{}", formatted.join("  "));
            }
        }
        QueryFormat::Json => {
            let arr: Vec<serde_json::Value> = rows
                .iter()
                .map(|row| {
                    let mut obj = serde_json::Map::new();
                    for (name, val) in col_names.iter().zip(row) {
                        obj.insert(name.to_lowercase(), serde_json::Value::String(val.clone()));
                    }
                    serde_json::Value::Object(obj)
                })
                .collect();
            let json = serde_json::to_string_pretty(&arr).unwrap_or_default();
            println!("{json}");
        }
        QueryFormat::Csv => {
            println!("{}", col_names.join(","));
            for row in rows {
                let escaped: Vec<String> = row
                    .iter()
                    .map(|v| {
                        if v.contains(',') || v.contains('"') || v.contains('\n') {
                            format!("\"{}\"", v.replace('"', "\"\""))
                        } else {
                            v.clone()
                        }
                    })
                    .collect();
                println!("{}", escaped.join(","));
            }
        }
    }
}

/// Garbage-collect old session data.
///
/// Deletes events, dead sessions, or specific sessions based on the flags.
/// Runs `VACUUM` when significant data is removed.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or a delete fails.
pub fn run_gc(
    conn: &rusqlite::Connection,
    older_than: Option<&str>,
    dead: bool,
    session_id: Option<&str>,
    sidecars: bool,
) -> Result<()> {
    let mut total_events_deleted: usize = 0;
    let mut sessions_deleted: usize = 0;
    // --older-than: delete old events and filter history
    if let Some(duration_str) = older_than {
        let cutoff = parse_since(duration_str)?;
        let cutoff_str = cutoff.to_rfc3339();
        let events_removed =
            conn.execute("DELETE FROM events WHERE timestamp < ?1", [&cutoff_str])?;
        total_events_deleted += events_removed;

        let filters_removed = conn.execute(
            "DELETE FROM filter_history WHERE created_at < ?1",
            [&cutoff_str],
        )?;

        println!(
            "Deleted {events_removed} event{}{} older than {duration_str}",
            if events_removed == 1 { "" } else { "s" },
            if filters_removed > 0 {
                format!(", {filters_removed} filter history entries")
            } else {
                String::new()
            },
        );
    }

    // --dead: detect crashed sessions, then delete all dead sessions
    if dead {
        // Mark crashed sessions (alive in DB but PID gone)
        let crashed: Vec<String> = {
            let mut stmt = conn.prepare("SELECT id, pid FROM sessions WHERE alive = 1")?;
            let mut rows = stmt.query([])?;
            let mut ids = Vec::new();
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let pid: u32 = row.get(1)?;
                if !session::is_process_alive(pid) {
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

        // Count events that will be cascade-deleted
        let dead_events: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE session_id IN \
                 (SELECT id FROM sessions WHERE alive = 0)",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let removed = conn.execute("DELETE FROM sessions WHERE alive = 0", [])?;
        sessions_deleted += removed;
        total_events_deleted += dead_events;

        println!(
            "Deleted {removed} dead session{} ({dead_events} event{})",
            if removed == 1 { "" } else { "s" },
            if dead_events == 1 { "" } else { "s" },
        );
    }

    // --session: delete a specific session
    if let Some(sid) = session_id {
        let resolved = resolve_session_id(conn, sid)?;

        let event_count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE session_id = ?1",
                [&resolved.id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        conn.execute("DELETE FROM sessions WHERE id = ?1", [&resolved.id])?;
        sessions_deleted += 1;
        total_events_deleted += event_count;

        // Clean up socket directory
        let socket_dir = session::sessions_dir().join(&resolved.id);
        let _ = std::fs::remove_dir_all(&socket_dir);

        println!(
            "Deleted session {} ({event_count} event{})",
            &resolved.id[..8.min(resolved.id.len())],
            if event_count == 1 { "" } else { "s" },
        );
    }

    if sidecars {
        gc_restore_sidecars(conn)?;
    } // --sidecars
    // Snapshot cleanup (fixed 7-day retention, always runs)
    let snapshots_deleted = gc_expired_snapshots(conn)?;

    if older_than.is_none() && !dead && session_id.is_none() && !sidecars && snapshots_deleted == 0
    {
        println!("Nothing to do. Use --older-than, --dead, --session, or --sidecars.");
    }

    // VACUUM if significant data was deleted
    if total_events_deleted > 1000 || sessions_deleted > 0 {
        let size_before = db_file_size();
        conn.execute_batch("VACUUM")?;
        let size_after = db_file_size();

        if let (Some(before), Some(after)) = (size_before, size_after) {
            let saved = before.saturating_sub(after);
            if saved > 0 {
                println!("Database vacuumed (saved {})", format_bytes(saved));
            }
        }
    }

    Ok(())
}

/// Remove sidecar files for all restore snapshots regardless of age.
///
/// Deletes sidecar files whose content matches the snapshot. Sidecars
/// whose content differs are left in place with a warning.
///
/// Returns the number of sidecars removed.
///
/// # Errors
///
/// Returns an error if the database query fails.
fn gc_restore_sidecars(conn: &rusqlite::Connection) -> Result<usize> {
    let mut stmt =
        conn.prepare("SELECT id, file_path, content FROM snapshots WHERE source = 'restore'")?;
    let rows: Vec<(i64, String, Vec<u8>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<Vec<_>, _>>()?;

    let mut removed: usize = 0;
    for (id, file_path, content) in &rows {
        let sidecar = crate::restore::sidecar_path(Path::new(file_path), *id);
        if sidecar.exists() {
            match std::fs::read(&sidecar) {
                Ok(ref disk_content) if disk_content == content => {
                    let _ = std::fs::remove_file(&sidecar);
                    removed += 1;
                    println!("Removed {}", sidecar.display());
                }
                Ok(_) => {
                    println!(
                        "sidecar {} differs from snapshot #{id} — not deleted.",
                        sidecar.display(),
                    );
                }
                Err(_) => {}
            }
        }
    }

    if removed > 0 {
        println!(
            "Removed {removed} sidecar{}.",
            if removed == 1 { "" } else { "s" },
        );
    }

    Ok(removed)
}

/// Clean up expired snapshots (fixed 7-day retention).
///
/// Deletes all snapshot rows older than 7 days. For restore snapshots,
/// also removes matching sidecar files from disk. Sidecars whose content
/// differs from the snapshot are left in place with a warning.
///
/// Returns the number of snapshot rows deleted.
///
/// # Errors
///
/// Returns an error if the database query or delete fails.
fn gc_expired_snapshots(conn: &rusqlite::Connection) -> Result<usize> {
    // Process restore sidecars before deleting rows.
    let mut stmt = conn.prepare(
        "SELECT id, file_path, content FROM snapshots \
         WHERE source = 'restore' AND created_at < datetime('now', '-7 days')",
    )?;
    let restore_rows: Vec<(i64, String, Vec<u8>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<Vec<_>, _>>()?;

    let mut sidecars_deleted: usize = 0;
    for (id, file_path, content) in &restore_rows {
        let sidecar = crate::restore::sidecar_path(Path::new(file_path), *id);
        if sidecar.exists() {
            match std::fs::read(&sidecar) {
                Ok(ref disk_content) if disk_content == content => {
                    let _ = std::fs::remove_file(&sidecar);
                    sidecars_deleted += 1;
                }
                Ok(_) => {
                    println!(
                        "sidecar {} differs from snapshot #{id} — not deleted.",
                        sidecar.display(),
                    );
                }
                Err(_) => {}
            }
        }
    }

    let snapshots_deleted = conn.execute(
        "DELETE FROM snapshots WHERE created_at < datetime('now', '-7 days')",
        [],
    )?;

    if snapshots_deleted > 0 {
        print!(
            "Deleted {snapshots_deleted} expired snapshot{}",
            if snapshots_deleted == 1 { "" } else { "s" },
        );
        if sidecars_deleted > 0 {
            print!(
                " ({sidecars_deleted} sidecar{})",
                if sidecars_deleted == 1 { "" } else { "s" },
            );
        }
        println!();
    }

    Ok(snapshots_deleted)
}

/// Get the database file size in bytes.
fn db_file_size() -> Option<u64> {
    std::fs::metadata(crate::db::db_path())
        .ok()
        .map(|m| m.len())
}

/// Format a byte count as a human-readable string.
#[allow(
    clippy::cast_precision_loss,
    reason = "byte counts are small enough for f64"
)]
fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Find session by ID or prefix.
///
/// # Errors
///
/// Returns an error if the session cannot be found.
pub fn find_session(conn: &rusqlite::Connection, id: &str) -> Result<session::SessionInfo> {
    // Try exact match first
    if let Some((s, _)) = session::get_session_with_conn(conn, id)? {
        return Ok(s);
    }

    // Try prefix match
    let sessions = session::list_sessions_with_conn(conn)?;
    let matches: Vec<_> = sessions
        .iter()
        .filter(|(s, _)| s.id.starts_with(id))
        .collect();

    match matches.len() {
        0 => anyhow::bail!("No session found matching '{id}'"),
        1 => Ok(matches[0].0.clone()),
        _ => {
            eprintln!("Multiple sessions match '{id}':");
            for (s, _) in matches {
                eprintln!("  {}", s.id);
            }
            anyhow::bail!("Please specify a more complete session ID")
        }
    }
}

/// Format a timestamp as "Xm ago" or similar.
#[must_use]
pub fn format_duration_ago(timestamp: chrono::DateTime<Utc>) -> String {
    let now = Utc::now();
    let duration = now.signed_duration_since(timestamp);

    if duration.num_hours() > 0 {
        format!(
            "{}h {}m ago",
            duration.num_hours(),
            duration.num_minutes() % 60
        )
    } else if duration.num_minutes() > 0 {
        format!("{}m ago", duration.num_minutes())
    } else {
        format!("{}s ago", duration.num_seconds())
    }
}

/// Print an event in raw JSON format.
fn print_event_raw(event: &SessionEvent) {
    let time = event.timestamp.with_timezone(&Local).format("%H:%M:%S");

    if let EventKind::ProtocolMessage {
        protocol,
        language,
        direction,
        message,
    } = &event.kind
    {
        let tag = match protocol {
            Protocol::Mcp => "[mcp]".to_string(),
            Protocol::Lsp => format!("[{}]", language.as_deref().unwrap_or("lsp")),
        };
        let arrow = match direction {
            Direction::Recv => "\u{2192}",
            Direction::Send => "\u{2190}",
        };
        println!("[{time}] {tag} {arrow}");
        let pretty = serde_json::to_string_pretty(message).unwrap_or_default();
        println!("{pretty}");
    } else {
        // For non-protocol events, print as JSON
        let json = serde_json::to_string_pretty(&event.kind).unwrap_or_default();
        println!("[{time}] {json}");
    }
}

/// Print an event with annotations and colors.
#[allow(clippy::too_many_lines, reason = "Match arms for each event kind")]
fn print_event_annotated(event: &SessionEvent, colors: &ColorConfig, term_width: usize) {
    let time = event.timestamp.with_timezone(&Local).format("%H:%M:%S");
    let time_str = colors.dim(&format!("[{time}]"));

    match &event.kind {
        EventKind::Started => {
            println!("{time_str} Session started");
        }
        EventKind::Shutdown => {
            println!("{time_str} Session shutting down");
        }
        EventKind::ServerState { language, state } => {
            let lang = colors.cyan(language);
            println!("{time_str} {lang}: {state}");
        }
        EventKind::Progress {
            language,
            title,
            message,
            percentage,
        } => {
            let lang = colors.cyan(language);
            let pct = percentage.map(|p| format!(" {p}%")).unwrap_or_default();
            let msg = message
                .as_ref()
                .map(|m| format!(" ({m})"))
                .unwrap_or_default();
            println!("{time_str} {lang}: {title}{pct}{msg}");
        }
        EventKind::ProgressEnd { language } => {
            let lang = colors.cyan(language);
            println!("{time_str} {lang}: Ready");
        }
        EventKind::ToolCall { tool, file, .. } => {
            let arrow = colors.green("→");
            let file_str = file
                .as_ref()
                .map(|f| format!(" on {f}"))
                .unwrap_or_default();
            println!("{time_str} {arrow} {tool}{file_str}");
        }
        EventKind::ToolResult {
            tool,
            success,
            duration_ms,
            ..
        } => {
            let arrow = colors.blue("←");
            let status = if *success {
                "ok".to_string()
            } else {
                colors.red("error")
            };
            println!("{time_str} {arrow} {tool} -> {status} ({duration_ms}ms)");
        }
        EventKind::Diagnostics {
            file,
            count,
            preview,
        } => {
            let basename = std::path::Path::new(file)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(file);
            if *count == 0 {
                let check = colors.green("ok");
                println!("{time_str} {basename}: {check}");
            } else {
                let label = colors.yellow(&format!(
                    "{count} diagnostic{}",
                    if *count == 1 { "" } else { "s" }
                ));
                let detail = if preview.is_empty() {
                    String::new()
                } else {
                    let max_len = term_width.saturating_sub(14 + basename.len() + 20);
                    format!(" -- {}", cli::truncate(preview, max_len))
                };
                println!("{time_str} {basename}: {label}{detail}");
            }
        }
        EventKind::ProtocolMessage {
            protocol,
            language,
            direction,
            message,
        } => {
            let tag = match protocol {
                Protocol::Mcp => "[mcp]".to_string(),
                Protocol::Lsp => format!("[{}]", language.as_deref().unwrap_or("lsp")),
            };
            let arrow_colored = match direction {
                Direction::Recv => colors.green("\u{2192}"),
                Direction::Send => colors.blue("\u{2190}"),
            };

            // Extract meaningful info from protocol message
            let summary = extract_mcp_summary(message, colors);

            // Calculate available width for message
            // Format: [HH:MM:SS] [tag] → summary
            let prefix_len = 10 + tag.len() + 2 + 2; // [time] + tag + arrow + spaces
            let max_summary_len = term_width.saturating_sub(prefix_len);

            let summary = cli::truncate(&summary, max_summary_len);
            println!("{time_str} {tag} {arrow_colored} {summary}");

            // Check for errors in response
            if matches!(direction, Direction::Send)
                && let Some(obj) = message.as_object()
                && obj.contains_key("error")
                && let Some(error) = obj.get("error")
            {
                let err_msg = error
                    .get("message")
                    .and_then(|m: &serde_json::Value| m.as_str())
                    .unwrap_or("Unknown error");
                println!("    {}", colors.red(&format!("Error: {err_msg}")));
            }
        }
    }
}

/// Extract a human-readable summary from an MCP message.
fn extract_mcp_summary(message: &serde_json::Value, colors: &ColorConfig) -> String {
    let Some(obj) = message.as_object() else {
        return message.to_string();
    };

    // Check if this is a request (has method)
    obj.get("method").and_then(|m| m.as_str()).map_or_else(
        || {
            // Check if this is a response (has result or error)
            if obj.contains_key("result") || obj.contains_key("error") {
                let id = obj.get("id").map(|i| format!("#{i}")).unwrap_or_default();

                if obj.contains_key("error") {
                    format!("{} {}", colors.red("error"), id)
                } else {
                    format!("result {id}")
                }
            } else {
                // Fallback: show compact JSON
                serde_json::to_string(message).unwrap_or_default()
            }
        },
        |method| {
            let id = obj.get("id").map(|i| format!("#{i}")).unwrap_or_default();

            // Extract params summary based on method
            let params_summary = match method {
                "tools/call" => {
                    if let Some(params) = obj.get("params")
                        && let Some(name) = params.get("name").and_then(|n| n.as_str())
                    {
                        // Try to get file argument if present
                        let file_info = params
                            .get("arguments")
                            .and_then(|a| a.get("file_path").or_else(|| a.get("path")))
                            .and_then(|f| f.as_str())
                            .map(|f| {
                                // Just show filename, not full path
                                std::path::Path::new(f)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or(f)
                            })
                            .map(|f| format!(" ({f})"))
                            .unwrap_or_default();
                        format!("{}{}", colors.cyan(name), file_info)
                    } else {
                        String::new()
                    }
                }
                "initialize" => {
                    if let Some(params) = obj.get("params")
                        && let Some(info) = params.get("clientInfo")
                        && let Some(name) = info.get("name").and_then(|n| n.as_str())
                    {
                        format!("from {name}")
                    } else {
                        String::new()
                    }
                }
                _ => String::new(),
            };

            if params_summary.is_empty() {
                format!("{method} {id}")
            } else {
                format!("{method} {params_summary} {id}")
            }
        },
    )
}

/// Print an event in human-readable format (used by `run_status`).
fn print_event(event: &SessionEvent) {
    let colors = ColorConfig::new(false);
    let term_width = cli::terminal_width();
    print_event_annotated(event, &colors, term_width);
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::session;

    /// Open an isolated test database in a tempdir.
    fn test_db() -> (tempfile::TempDir, std::path::PathBuf, rusqlite::Connection) {
        let dir = tempfile::tempdir().expect("failed to create tempdir for test DB");
        let path = dir.path().join("catenary").join("catenary.db");
        let conn = crate::db::open_and_migrate_at(&path).expect("failed to open test DB");
        (dir, path, conn)
    }

    /// Create a session backed by the database at `db_path`.
    fn create_session(
        db_path: &std::path::Path,
        workspace: &str,
    ) -> anyhow::Result<session::Session> {
        let arc = std::sync::Arc::new(std::sync::Mutex::new(crate::db::open_and_migrate_at(
            db_path,
        )?));
        session::Session::create_with_conn(workspace, arc)
    }

    // ── parse_since tests ────────────────────────────────────────────

    #[test]
    fn test_parse_since_hours() -> anyhow::Result<()> {
        let cutoff = parse_since("1h")?;
        let diff = Utc::now().signed_duration_since(cutoff);
        // Should be approximately 1 hour (allow 5s tolerance)
        assert!(
            diff.num_seconds() >= 3595 && diff.num_seconds() <= 3605,
            "expected ~3600s, got {}s",
            diff.num_seconds()
        );
        Ok(())
    }

    #[test]
    fn test_parse_since_days() -> anyhow::Result<()> {
        let cutoff = parse_since("7d")?;
        let diff = Utc::now().signed_duration_since(cutoff);
        let expected = 7 * 86400;
        assert!(
            diff.num_seconds() >= expected - 5 && diff.num_seconds() <= expected + 5,
            "expected ~{expected}s, got {}s",
            diff.num_seconds()
        );
        Ok(())
    }

    #[test]
    fn test_parse_since_minutes() -> anyhow::Result<()> {
        let cutoff = parse_since("30m")?;
        let diff = Utc::now().signed_duration_since(cutoff);
        assert!(
            diff.num_seconds() >= 1795 && diff.num_seconds() <= 1805,
            "expected ~1800s, got {}s",
            diff.num_seconds()
        );
        Ok(())
    }

    #[test]
    fn test_parse_since_today() -> anyhow::Result<()> {
        let cutoff = parse_since("today")?;
        let now = Utc::now();
        // Cutoff should be before now
        assert!(cutoff <= now);
        // And within the last 24 hours
        assert!(now.signed_duration_since(cutoff).num_hours() < 24);
        Ok(())
    }

    #[test]
    fn test_parse_since_invalid() {
        assert!(parse_since("abc").is_err());
        assert!(parse_since("").is_err());
        assert!(parse_since("5x").is_err());
    }

    // ── query tests ─────────────────────────────────────────────────

    #[test]
    #[ignore = "05b: uses removed EventBroadcaster"]
    fn test_query_with_kind_filter() {}

    #[test]
    #[ignore = "05b: uses removed EventBroadcaster"]
    fn test_query_with_search() {}

    // ── gc tests ────────────────────────────────────────────────────

    #[test]
    fn test_gc_dead_sessions() -> anyhow::Result<()> {
        let (_dir, path, conn) = test_db();

        let session = create_session(&path, "/tmp/test-gc-dead")?;
        let id = session.info.id.clone();
        drop(session); // marks as dead

        // Verify it exists as dead
        let found = session::get_session_with_conn(&conn, &id)?;
        assert!(found.is_some(), "session should exist");
        assert!(!found.expect("checked above").1, "session should be dead");

        // Run gc --dead
        run_gc(&conn, None, true, None, false)?;

        // Should be gone
        assert!(
            session::get_session_with_conn(&conn, &id)?.is_none(),
            "dead session should be deleted"
        );
        Ok(())
    }

    #[test]
    fn test_gc_specific_session() -> anyhow::Result<()> {
        let (_dir, path, conn) = test_db();

        let s1 = create_session(&path, "/tmp/test-gc-specific-1")?;
        let id1 = s1.info.id.clone();
        let s2 = create_session(&path, "/tmp/test-gc-specific-2")?;
        let id2 = s2.info.id.clone();
        drop(s1);
        drop(s2);

        // Delete only s1
        run_gc(&conn, None, false, Some(&id1), false)?;

        assert!(
            session::get_session_with_conn(&conn, &id1)?.is_none(),
            "targeted session should be deleted"
        );
        assert!(
            session::get_session_with_conn(&conn, &id2)?.is_some(),
            "other session should survive"
        );

        session::delete_session_data_with_conn(&conn, &id2)?;
        Ok(())
    }

    #[test]
    fn test_gc_no_flags_is_noop() -> anyhow::Result<()> {
        let (_dir, _path, conn) = test_db();
        // Should not error
        run_gc(&conn, None, false, None, false)?;
        Ok(())
    }

    #[test]
    fn test_gc_sidecar_identical() -> anyhow::Result<()> {
        let (_db_dir, _path, conn) = test_db();
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("test.rs");
        let file_str = file.to_string_lossy().to_string();

        let content = b"original content";

        // Insert an expired restore snapshot (older than 7 days).
        conn.execute(
            "INSERT INTO snapshots \
                 (file_path, content, source, created_at) \
             VALUES (?1, ?2, 'restore', datetime('now', '-8 days'))",
            rusqlite::params![&file_str, content.as_slice()],
        )?;
        let id = conn.last_insert_rowid();

        // Create sidecar with identical content.
        let sidecar = crate::restore::sidecar_path(&file, id);
        std::fs::write(&sidecar, content).expect("write sidecar");

        gc_expired_snapshots(&conn)?;

        // Sidecar should be deleted.
        assert!(!sidecar.exists(), "sidecar should be deleted");

        // Snapshot row should be deleted.
        let count: usize = conn.query_row(
            "SELECT COUNT(*) FROM snapshots WHERE id = ?1",
            [id],
            |row| row.get(0),
        )?;
        assert_eq!(count, 0, "snapshot row should be deleted");
        Ok(())
    }

    #[test]
    fn test_gc_sidecar_modified() -> anyhow::Result<()> {
        let (_db_dir, _path, conn) = test_db();
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("test.rs");
        let file_str = file.to_string_lossy().to_string();

        let content = b"original content";

        conn.execute(
            "INSERT INTO snapshots \
                 (file_path, content, source, created_at) \
             VALUES (?1, ?2, 'restore', datetime('now', '-8 days'))",
            rusqlite::params![&file_str, content.as_slice()],
        )?;
        let id = conn.last_insert_rowid();

        // Create sidecar with different content.
        let sidecar = crate::restore::sidecar_path(&file, id);
        std::fs::write(&sidecar, b"modified by user").expect("write sidecar");

        gc_expired_snapshots(&conn)?;

        // Sidecar should NOT be deleted.
        assert!(sidecar.exists(), "modified sidecar should survive");

        // Snapshot row should still be deleted.
        let count: usize = conn.query_row(
            "SELECT COUNT(*) FROM snapshots WHERE id = ?1",
            [id],
            |row| row.get(0),
        )?;
        assert_eq!(count, 0, "snapshot row should be deleted regardless");
        Ok(())
    }

    #[test]
    fn test_gc_sidecar_missing() -> anyhow::Result<()> {
        let (_db_dir, _path, conn) = test_db();
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("test.rs");
        let file_str = file.to_string_lossy().to_string();

        conn.execute(
            "INSERT INTO snapshots \
                 (file_path, content, source, created_at) \
             VALUES (?1, ?2, 'restore', datetime('now', '-8 days'))",
            rusqlite::params![&file_str, b"content".as_slice()],
        )?;
        let id = conn.last_insert_rowid();

        // No sidecar on disk — should not error.
        gc_expired_snapshots(&conn)?;

        let count: usize = conn.query_row(
            "SELECT COUNT(*) FROM snapshots WHERE id = ?1",
            [id],
            |row| row.get(0),
        )?;
        assert_eq!(count, 0, "snapshot row should be deleted");
        Ok(())
    }

    #[test]
    fn test_gc_non_restore_snapshot() -> anyhow::Result<()> {
        let (_db_dir, _path, conn) = test_db();

        conn.execute(
            "INSERT INTO snapshots \
                 (file_path, content, source, pattern, count, created_at) \
             VALUES ('src/test.rs', X'00', 'replace', '1 edits', 1, datetime('now', '-8 days'))",
            [],
        )?;
        let id = conn.last_insert_rowid();

        gc_expired_snapshots(&conn)?;

        // Row should be deleted (no sidecar check for non-restore).
        let count: usize = conn.query_row(
            "SELECT COUNT(*) FROM snapshots WHERE id = ?1",
            [id],
            |row| row.get(0),
        )?;
        assert_eq!(count, 0, "replace snapshot row should be deleted");
        Ok(())
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(2_621_440), "2.5 MB");
    }
}

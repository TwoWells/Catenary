// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! MCP tools for per-file diagnostic batching.
//!
//! `start_editing` signals that the agent intends to make multiple edits to a
//! file. Diagnostics are suppressed until `done_editing` is called, at which
//! point the final state is checked and diagnostics are returned.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};

use super::diagnostics_server::DiagnosticsServer;
use super::handler::display_path;
use crate::db;

/// Resolves a file path against workspace roots.
///
/// Absolute paths pass through. Relative paths are resolved against each root
/// in order — the first root where the file exists wins. If the file doesn't
/// exist under any root, returns an error listing the roots that were checked.
fn resolve_in_roots(file: &str, roots: &[PathBuf]) -> Result<PathBuf> {
    let path = PathBuf::from(file);
    if path.is_absolute() {
        return Ok(path);
    }

    // Try each workspace root — first existing file wins.
    for root in roots {
        let candidate = root.join(&path);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    // File doesn't exist under any root. If there's exactly one root,
    // resolve against it (could be a new file). Otherwise, report the
    // mismatch so the agent can provide an absolute path.
    if roots.len() == 1 {
        return Ok(roots[0].join(&path));
    }

    let root_list: Vec<_> = roots.iter().map(|r| r.to_string_lossy().to_string()).collect();
    Err(anyhow!(
        "cannot resolve relative path \"{file}\" — file not found under workspace roots: [{}]. \
         Use an absolute path instead.",
        root_list.join(", ")
    ))
}

/// Handles `start_editing` and `done_editing` MCP tool calls.
///
/// Manages per-file editing state in SQLite and delegates diagnostic
/// collection to `DiagnosticsServer` on `done_editing`.
pub struct EditingServer {
    diagnostics: Arc<DiagnosticsServer>,
    session_id: String,
}

impl EditingServer {
    /// Creates a new `EditingServer`.
    pub fn new(diagnostics: Arc<DiagnosticsServer>, session_id: String) -> Self {
        Self {
            diagnostics,
            session_id,
        }
    }

    /// Marks a file as being edited. Diagnostics will be suppressed until
    /// [`done_editing`](Self::done_editing) is called.
    ///
    /// Returns a status message indicating whether the file was newly marked
    /// or was already being edited.
    ///
    /// # Errors
    ///
    /// Returns an error if the file path cannot be resolved or the database
    /// operation fails.
    pub fn start_editing(
        &self,
        file: &str,
        roots: &[PathBuf],
    ) -> Result<String> {
        let path = resolve_in_roots(file, roots)?;
        let abs = path.to_string_lossy();
        let display = display_path(&abs, roots);

        let conn = db::open()?;
        let created = db::start_editing(&conn, &abs, &self.session_id, "")?;

        if created {
            Ok(format!(
                "editing {display} \u{2014} diagnostics deferred until done_editing"
            ))
        } else {
            Ok(format!("already editing {display}"))
        }
    }

    /// Marks a file as done being edited and returns diagnostics for its
    /// final state.
    ///
    /// # Errors
    ///
    /// Returns an error if the file is not being edited, the path cannot be
    /// resolved, or diagnostics collection fails.
    pub async fn done_editing(
        &self,
        file: &str,
        roots: &[PathBuf],
    ) -> Result<String> {
        let path = resolve_in_roots(file, roots)?;
        let abs = path.to_string_lossy();
        let display = display_path(&abs, roots);

        let conn = db::open()?;
        db::done_editing(&conn, &abs, &self.session_id, "")?;
        drop(conn);

        let result = self
            .diagnostics
            .process_file(&abs, 0)
            .await
            .map_err(|e| anyhow!("diagnostics failed for {display}: {e}"))?;

        if result.count == 0 {
            Ok(format!("done editing {display} [clean]"))
        } else {
            Ok(format!("done editing {display}\n{}", result.content))
        }
    }
}

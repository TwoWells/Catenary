/*
 * Copyright (C) 2026 Mark Wells Dev
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

//! File I/O tool handlers: `read_file`, `write_file`, `edit_file`, `list_directory`.
//!
//! All file operations validate paths against workspace roots before access.
//! Write/edit operations return LSP diagnostics automatically so models
//! cannot proceed unaware of errors they introduced.

use anyhow::{Result, anyhow};
use lsp_types::Diagnostic;
use serde::Deserialize;
use std::fmt::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tracing::debug;

use super::DocumentNotification;
use super::handler::LspBridgeHandler;
use crate::lsp::{DIAGNOSTICS_TIMEOUT, LspClient};
use crate::mcp::CallToolResult;

/// Input for `read_file`.
#[derive(Debug, Deserialize)]
pub struct ReadFileInput {
    /// Path to the file (absolute or relative).
    pub file: String,
    /// Starting line offset (1-indexed). If provided, reads from this line.
    pub offset: Option<usize>,
    /// Maximum number of lines to return.
    pub limit: Option<usize>,
}

/// Input for `write_file`.
#[derive(Debug, Deserialize)]
pub struct WriteFileInput {
    /// Path to the file (absolute or relative).
    pub file: String,
    /// Content to write.
    pub content: String,
}

/// Input for `edit_file`.
#[derive(Debug, Deserialize)]
pub struct EditFileInput {
    /// Path to the file (absolute or relative).
    pub file: String,
    /// The exact text to find in the file.
    pub old_string: String,
    /// The text to replace it with.
    pub new_string: String,
}

/// Input for `list_directory`.
#[derive(Debug, Deserialize)]
pub struct ListDirectoryInput {
    /// Path to the directory (absolute or relative).
    pub path: String,
}

/// Maximum bytes to check for binary file detection.
const BINARY_CHECK_BYTES: usize = 8192;

impl LspBridgeHandler {
    /// Handles the `read_file` tool call.
    pub(super) fn handle_read_file(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input: ReadFileInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let path = Self::resolve_path(&input.file)?;

        debug!("read_file: {}", input.file);

        let canonical = self
            .runtime
            .block_on(self.path_validator.read())
            .validate_read(&path)?;

        let content = self
            .runtime
            .block_on(tokio::fs::read_to_string(&canonical))
            .map_err(|e| anyhow!("Failed to read file: {e}"))?;

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        // Apply offset/limit
        let offset = input.offset.unwrap_or(1).saturating_sub(1); // Convert 1-indexed to 0-indexed
        let limit = input
            .limit
            .unwrap_or_else(|| total_lines.saturating_sub(offset));
        let end = (offset + limit).min(total_lines);
        let selected = &lines[offset.min(total_lines)..end];

        // Format with line numbers
        let mut result = String::new();
        for (i, line) in selected.iter().enumerate() {
            let line_num = offset + i + 1;
            let _ = writeln!(result, "{line_num:>6}\t{line}");
        }

        if offset > 0 || end < total_lines {
            let _ = writeln!(
                result,
                "\n(Showing lines {}-{} of {total_lines})",
                offset + 1,
                end
            );
        }

        // Best-effort diagnostics
        let diagnostics_section = self.fetch_diagnostics_for_path(&canonical);
        if !diagnostics_section.is_empty() {
            let _ = write!(result, "\n{diagnostics_section}");
        }

        Ok(CallToolResult::text(result))
    }

    /// Handles the `write_file` tool call.
    pub(super) fn handle_write_file(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input: WriteFileInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let path = Self::resolve_path(&input.file)?;

        debug!("write_file: {}", input.file);

        let canonical = self
            .runtime
            .block_on(self.path_validator.read())
            .validate_write(&path)?;

        // Create parent directories if needed
        if let Some(parent) = canonical.parent() {
            self.runtime
                .block_on(tokio::fs::create_dir_all(parent))
                .map_err(|e| anyhow!("Failed to create parent directories: {e}"))?;
        }

        // Write the file
        self.runtime
            .block_on(tokio::fs::write(&canonical, &input.content))
            .map_err(|e| anyhow!("Failed to write file: {e}"))?;

        // Check if this file introduces a new language for the run tool
        if let Some(ref run_tool) = self.run_tool
            && self
                .runtime
                .block_on(run_tool.write())
                .maybe_detect_language(&canonical)
            && let Some(ref flag) = self.tools_changed_flag
        {
            flag.store(true, Ordering::Release);
        }

        let line_count = input.content.lines().count();
        let rel_path = self.relative_display_path(&canonical);

        // Notify LSP and get diagnostics
        let diagnostics_section = self.notify_lsp_and_get_diagnostics(&canonical, &input.content);

        let mut result = format!("Wrote {line_count} lines to {rel_path}");
        if !diagnostics_section.is_empty() {
            let _ = write!(result, "\n\n{diagnostics_section}");
        }

        Ok(CallToolResult::text(result))
    }

    /// Handles the `edit_file` tool call.
    pub(super) fn handle_edit_file(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input: EditFileInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        if input.old_string.is_empty() {
            return Err(anyhow!("old_string cannot be empty"));
        }

        if input.old_string == input.new_string {
            return Ok(CallToolResult::text(
                "No changes needed (old_string == new_string)",
            ));
        }

        let path = Self::resolve_path(&input.file)?;

        debug!("edit_file: {}", input.file);

        let canonical = self
            .runtime
            .block_on(self.path_validator.read())
            .validate_write(&path)?;

        // Read current content
        let content = self
            .runtime
            .block_on(tokio::fs::read_to_string(&canonical))
            .map_err(|e| anyhow!("Failed to read file: {e}"))?;

        // Check for binary content
        let check_len = content.len().min(BINARY_CHECK_BYTES);
        if content.as_bytes()[..check_len].contains(&0) {
            return Err(anyhow!("Cannot edit binary file: {}", input.file));
        }

        // Find old_string
        let match_count = content.matches(&input.old_string).count();
        match match_count {
            0 => {
                return Err(anyhow!(
                    "old_string not found in {}. No changes made.",
                    input.file
                ));
            }
            1 => {} // Exactly one match — proceed
            n => {
                return Err(anyhow!(
                    "old_string appears {n} times in {}. Provide more surrounding context to make the match unique.",
                    input.file
                ));
            }
        }

        // Replace
        let new_content = content.replacen(&input.old_string, &input.new_string, 1);

        // Write
        self.runtime
            .block_on(tokio::fs::write(&canonical, &new_content))
            .map_err(|e| anyhow!("Failed to write file: {e}"))?;

        let rel_path = self.relative_display_path(&canonical);

        // Notify LSP and get diagnostics
        let diagnostics_section = self.notify_lsp_and_get_diagnostics(&canonical, &new_content);

        let mut result = format!("Edited {rel_path}");
        if !diagnostics_section.is_empty() {
            let _ = write!(result, "\n\n{diagnostics_section}");
        }

        Ok(CallToolResult::text(result))
    }

    /// Handles the `list_directory` tool call.
    pub(super) fn handle_list_directory(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input: ListDirectoryInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let path = Self::resolve_path(&input.path)?;

        debug!("list_directory: {}", input.path);

        let canonical = self
            .runtime
            .block_on(self.path_validator.read())
            .validate_read(&path)?;

        if !canonical.is_dir() {
            return Err(anyhow!("Not a directory: {}", input.path));
        }

        let entries = self.runtime.block_on(async {
            let mut dir = tokio::fs::read_dir(&canonical)
                .await
                .map_err(|e| anyhow!("Failed to read directory: {e}"))?;

            let mut dirs = Vec::new();
            let mut files = Vec::new();
            let mut symlinks = Vec::new();

            while let Some(entry) = dir
                .next_entry()
                .await
                .map_err(|e| anyhow!("Failed to read directory entry: {e}"))?
            {
                let name = entry.file_name().to_string_lossy().to_string();

                // Use symlink_metadata to avoid following symlinks
                let metadata = entry
                    .path()
                    .symlink_metadata()
                    .map_err(|e| anyhow!("Failed to read metadata for {name}: {e}"))?;

                if metadata.file_type().is_symlink() {
                    // Show symlink with its target
                    let target = std::fs::read_link(entry.path())
                        .map_or_else(|_| "?".to_string(), |t| t.to_string_lossy().to_string());
                    symlinks.push(format!("{name} -> {target}"));
                } else if metadata.is_dir() {
                    dirs.push(format!("{name}/"));
                } else {
                    let size = metadata.len();
                    files.push((name, size));
                }
            }

            // Sort alphabetically
            dirs.sort();
            files.sort_by(|a, b| a.0.cmp(&b.0));
            symlinks.sort();

            Ok::<_, anyhow::Error>((dirs, files, symlinks))
        })?;

        let (dirs, files, symlinks) = entries;
        let mut result = String::new();

        for d in &dirs {
            let _ = writeln!(result, "{d}");
        }
        for (name, size) in &files {
            let _ = writeln!(result, "{name}  ({size} bytes)");
        }
        for s in &symlinks {
            let _ = writeln!(result, "{s}");
        }

        if result.is_empty() {
            result = "Directory is empty".to_string();
        }

        Ok(CallToolResult::text(result))
    }

    /// Fetches diagnostics for a path, returning a formatted string.
    /// Returns empty string if no LSP server is configured for the language.
    /// Returns an explicit warning if the server is dead or unresponsive.
    fn fetch_diagnostics_for_path(&self, path: &Path) -> String {
        let result: Result<String> = self.runtime.block_on(async {
            // Try to get the client — if no server configured, this errors
            let client_mutex: Arc<Mutex<LspClient>> = match self.get_client_for_path(path).await {
                Ok(c) => c,
                Err(_) => return Ok(String::new()), // No LSP server for this language
            };

            let mut doc_manager = self.doc_manager.lock().await;
            let client = client_mutex.lock().await;
            let lang = client.language().to_string();

            if !client.is_alive() {
                return Ok(format!(
                    "[{lang}] server is not running \u{2014} diagnostics unavailable"
                ));
            }

            let uri = doc_manager.uri_for_path(path)?;

            if let Some(notification) = doc_manager.ensure_open(path).await? {
                // Snapshot generation *before* sending the change
                let snapshot = client.diagnostics_generation(&uri).await;

                match notification {
                    DocumentNotification::Open(params) => {
                        client.did_open(params).await?;
                    }
                    DocumentNotification::Change(params) => {
                        client.did_change(params).await?;
                    }
                }

                drop(doc_manager);

                // Wait for diagnostics that reflect our change
                if !client
                    .wait_for_diagnostics_update(&uri, snapshot, DIAGNOSTICS_TIMEOUT)
                    .await
                {
                    return Ok(format!(
                        "[{lang}] server stopped responding \u{2014} diagnostics unavailable"
                    ));
                }
            } else {
                drop(doc_manager);
            }

            let diagnostics = client.get_diagnostics(&uri).await;
            drop(client);
            if diagnostics.is_empty() {
                Ok(String::new())
            } else {
                Ok(format!(
                    "Diagnostics ({}):\n{}",
                    diagnostics.len(),
                    format_diagnostics_compact(&diagnostics)
                ))
            }
        });

        match result {
            Ok(s) => s,
            Err(e) => format!("Diagnostics error: {e}"),
        }
    }

    /// Notifies the LSP server of a file write and returns formatted diagnostics.
    /// Returns empty string if no LSP server is configured for the language.
    /// Returns an explicit warning if the server is dead or unresponsive.
    fn notify_lsp_and_get_diagnostics(&self, path: &Path, content: &str) -> String {
        let result: Result<String> = self.runtime.block_on(async {
            let client_mutex: Arc<Mutex<LspClient>> = match self.get_client_for_path(path).await {
                Ok(c) => c,
                Err(_) => return Ok(String::new()),
            };

            let mtime = tokio::fs::metadata(path)
                .await
                .and_then(|m| m.modified())
                .unwrap_or_else(|_| std::time::SystemTime::now());

            let mut doc_manager = self.doc_manager.lock().await;
            let notification = doc_manager.notify_external_write(path, content, mtime)?;
            let uri = doc_manager.uri_for_path(path)?;
            drop(doc_manager);

            let client = client_mutex.lock().await;
            let lang = client.language().to_string();

            if !client.is_alive() {
                return Ok(format!(
                    "[{lang}] server is not running \u{2014} diagnostics unavailable"
                ));
            }

            // Snapshot generation *before* sending the change
            let snapshot = client.diagnostics_generation(&uri).await;

            match notification {
                DocumentNotification::Open(params) => {
                    client.did_open(params).await?;
                }
                DocumentNotification::Change(params) => {
                    client.did_change(params).await?;
                }
            }

            // Wait for diagnostics that reflect our change
            if !client
                .wait_for_diagnostics_update(&uri, snapshot, DIAGNOSTICS_TIMEOUT)
                .await
            {
                return Ok(format!(
                    "[{lang}] server stopped responding \u{2014} diagnostics unavailable"
                ));
            }

            let diagnostics = client.get_diagnostics(&uri).await;
            drop(client);
            if diagnostics.is_empty() {
                Ok(String::new())
            } else {
                Ok(format!(
                    "Diagnostics ({}):\n{}",
                    diagnostics.len(),
                    format_diagnostics_compact(&diagnostics)
                ))
            }
        });

        match result {
            Ok(s) => s,
            Err(e) => format!("Diagnostics error: {e}"),
        }
    }

    /// Returns a display-friendly relative path against workspace roots.
    pub(super) fn relative_display_path(&self, path: &Path) -> String {
        let roots = self.runtime.block_on(self.client_manager.roots());
        for root in &roots {
            if let Ok(relative) = path.strip_prefix(root) {
                return relative.to_string_lossy().to_string();
            }
        }
        path.to_string_lossy().to_string()
    }
}

/// Formats diagnostics with line/column and severity.
fn format_diagnostics_compact(diagnostics: &[Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(|d| {
            let severity = match d.severity {
                Some(lsp_types::DiagnosticSeverity::ERROR) => "error",
                Some(lsp_types::DiagnosticSeverity::WARNING) => "warning",
                Some(lsp_types::DiagnosticSeverity::INFORMATION) => "info",
                Some(lsp_types::DiagnosticSeverity::HINT) => "hint",
                _ => "unknown",
            };
            let line = d.range.start.line + 1;
            let col = d.range.start.character + 1;
            let source = d.source.as_deref().unwrap_or("");
            let code = d
                .code
                .as_ref()
                .map(|c| match c {
                    lsp_types::NumberOrString::Number(n) => n.to_string(),
                    lsp_types::NumberOrString::String(s) => s.clone(),
                })
                .unwrap_or_default();

            if code.is_empty() {
                format!("  {line}:{col} [{severity}] {source}: {}", d.message)
            } else {
                format!(
                    "  {line}:{col} [{severity}] {source}({code}): {}",
                    d.message
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

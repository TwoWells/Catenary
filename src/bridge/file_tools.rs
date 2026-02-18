// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! File I/O tool handlers: `list_directory`.
//!
//! Path operations validate paths against workspace roots before access.

use anyhow::{Result, anyhow};
use serde::Deserialize;
use std::fmt::Write;

use super::handler::LspBridgeHandler;
use crate::mcp::CallToolResult;

/// Input for `list_directory`.
#[derive(Debug, Deserialize)]
pub struct ListDirectoryInput {
    /// Path to the directory (absolute or relative).
    pub path: String,
}

impl LspBridgeHandler {
    /// Handles the `list_directory` tool call.
    pub(super) fn handle_list_directory(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input: ListDirectoryInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let path = Self::resolve_path(&input.path)?;

        tracing::debug!("list_directory: {}", input.path);

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
}

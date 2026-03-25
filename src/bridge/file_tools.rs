// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Glob tool handler: unified file/directory/pattern browsing.
//!
//! The `glob` tool auto-detects intent from the pattern:
//! - File path → symbol outline with line counts
//! - Directory path → listing with outline symbols and gitignored section
//! - Glob pattern → match files across workspace roots with outlines

use anyhow::{Result, anyhow};
use globset::Glob;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::warn;

use super::filesystem_manager::{FilesystemManager, format_file_size};
use super::handler::{check_server_health, resolve_path};
use super::symbols::{format_symbol_kind, is_outline_kind};
use super::tool_server::ToolServer;
use crate::lsp::LspClientManager;

/// Input for the `glob` tool.
#[derive(Debug, Deserialize)]
pub struct GlobInput {
    /// File path, directory path, or glob pattern.
    pub pattern: String,
}

/// Outline symbols for a single file: `(name, kind_u32, 1-based line)`.
type OutlineSymbols = Vec<(String, u32, u32)>;

/// Extracts depth-0 outline symbols from a document symbol response (`Value`).
///
/// Handles both flat `SymbolInformation[]` and nested `DocumentSymbol[]`.
fn extract_outline_symbols(response: &Value) -> OutlineSymbols {
    let Some(arr) = response.as_array() else {
        return Vec::new();
    };

    let mut symbols = Vec::new();

    for item in arr {
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        let kind = item
            .get("kind")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0);

        if !is_outline_kind(kind) {
            continue;
        }

        // Flat SymbolInformation: location.range.start.line
        if let Some(line) = item
            .get("location")
            .and_then(|l| l.get("range"))
            .and_then(|r| r.get("start"))
            .and_then(|s| s.get("line"))
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
        {
            symbols.push((name.to_string(), kind, line + 1));
            continue;
        }

        // Nested DocumentSymbol: range.start.line (depth 0 only)
        if let Some(line) = item
            .get("range")
            .and_then(|r| r.get("start"))
            .and_then(|s| s.get("line"))
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
        {
            symbols.push((name.to_string(), kind, line + 1));
        }
    }

    symbols
}

/// Glob tool server: unified file/directory/pattern browsing with LSP symbols.
pub struct GlobServer {
    pub(super) client_manager: Arc<LspClientManager>,
    pub(super) fs_manager: Arc<FilesystemManager>,
    pub(super) notified_offline: Arc<std::sync::Mutex<HashSet<String>>>,
}

impl ToolServer for GlobServer {
    async fn execute(
        &self,
        params: &serde_json::Value,
        parent_id: Option<i64>,
    ) -> Result<serde_json::Value> {
        let input: GlobInput = serde_json::from_value(params.clone())
            .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let path = resolve_path(&input.pattern)?;
        let file_path = if path.is_file() {
            Some(path.clone())
        } else {
            None
        };

        // Wait for readiness and check health
        let health = if let Some(ref fp) = file_path {
            self.wait_for_server_ready(fp).await;
            let touched: Vec<String> = self.language_for_path(fp).await.into_iter().collect();
            check_server_health(&self.client_manager, &touched, &self.notified_offline).await
        } else {
            self.wait_for_all_servers_ready().await;
            let touched: Vec<String> = self
                .client_manager
                .clients()
                .await
                .keys()
                .cloned()
                .collect();
            check_server_health(&self.client_manager, &touched, &self.notified_offline).await
        };

        tracing::debug!("glob: {}", input.pattern);

        // Run pipeline
        let output = if path.is_file() {
            self.handle_glob_file(&path, parent_id).await
        } else if path.is_dir() {
            self.handle_glob_dir(&path, parent_id).await?
        } else {
            self.handle_glob_pattern(&input.pattern, parent_id).await?
        };

        // Prepend notification
        let text = if let Some(note) = health.notification {
            if output.is_empty() {
                note
            } else {
                format!("{note}\n\n{output}")
            }
        } else {
            output
        };

        Ok(Value::String(text))
    }
}

impl GlobServer {
    /// Waits for the server handling the given path to be ready.
    ///
    /// Dead servers are non-fatal — the wait completes and the caller
    /// uses [`check_server_health`] to detect the state.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "Client lock held across wait_ready call"
    )]
    async fn wait_for_server_ready(&self, path: &Path) {
        let Ok(client_mutex) = self.client_manager.get_client(path).await else {
            return;
        };

        let client = client_mutex.lock().await;
        let lang = client.language().to_string();
        let is_ready = client.wait_ready().await;
        drop(client);

        if !is_ready {
            warn!("[{lang}] server died \u{2014} tool will run in degraded mode");
        }
    }

    /// Waits for all active LSP servers to be ready.
    async fn wait_for_all_servers_ready(&self) {
        let clients = self.client_manager.clients().await;

        for (lang, client_mutex) in clients {
            if !client_mutex.lock().await.wait_ready().await {
                warn!("[{lang}] server died \u{2014} tool will run in degraded mode");
            }
        }
    }

    /// Returns the language key for a file path, matching the key used in
    /// `clients()`.
    async fn language_for_path(&self, path: &Path) -> Option<String> {
        let client_mutex = self.client_manager.get_client(path).await.ok()?;
        Some(client_mutex.lock().await.language().to_string())
    }

    /// File outline: header with line count + depth-0 outline symbols.
    ///
    /// Binary files show size instead of line count and skip LSP symbols.
    async fn handle_glob_file(&self, path: &Path, parent_id: Option<i64>) -> String {
        let mut result = String::new();
        let display = path.to_string_lossy();
        let metadata = std::fs::metadata(path).ok();

        if let Some(line_count) = metadata
            .as_ref()
            .and_then(|m| self.fs_manager.line_count(path, m))
        {
            let _ = writeln!(result, "{display}  ({line_count} lines)");
            if let Ok(symbols) = self.fetch_outline_symbols(path, parent_id).await {
                for (name, kind, line) in &symbols {
                    let kind_str = format_symbol_kind(*kind);
                    let _ = writeln!(result, "  [{kind_str}] {name} L{line}");
                }
            }
        } else {
            let size = metadata.map_or(0, |m| m.len());
            let _ = writeln!(result, "{display}  ({})", format_file_size(size));
        }

        result
    }

    /// Directory listing with outline symbols and gitignored section.
    #[allow(clippy::too_many_lines, reason = "Two-pass directory classification")]
    async fn handle_glob_dir(&self, dir: &Path, parent_id: Option<i64>) -> Result<String> {
        let canonical = dir
            .canonicalize()
            .map_err(|e| anyhow!("Path does not exist: {}: {e}", dir.display()))?;

        // Pass 1: gitignore-aware walk → non-ignored set
        let mut non_ignored: HashSet<PathBuf> = HashSet::new();
        let walker = WalkBuilder::new(&canonical)
            .max_depth(Some(1))
            .git_ignore(true)
            .hidden(false)
            .build();

        for entry in walker.flatten() {
            let entry_path = entry.into_path();
            if entry_path == canonical {
                continue;
            }
            non_ignored.insert(entry_path);
        }

        // Pass 2: read_dir → all entries
        let all_entries: Vec<_> = std::fs::read_dir(&canonical)
            .map_err(|e| anyhow!("Failed to read directory: {e}"))?
            .filter_map(std::result::Result::ok)
            .collect();

        let mut dirs = Vec::new();
        // (name, line_count, symbols, binary_size)
        let mut files: Vec<(String, usize, OutlineSymbols, Option<String>)> = Vec::new();
        let mut symlinks = Vec::new();
        let mut gitignored = Vec::new();

        for entry in &all_entries {
            let entry_path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let metadata = entry_path
                .symlink_metadata()
                .map_err(|e| anyhow!("Failed to read metadata for {name}: {e}"))?;

            if !non_ignored.contains(&entry_path) {
                if metadata.is_dir() {
                    gitignored.push(format!("{name}/"));
                } else {
                    gitignored.push(name);
                }
                continue;
            }

            if metadata.file_type().is_symlink() {
                let target = std::fs::read_link(&entry_path)
                    .map_or_else(|_| "?".to_string(), |t| t.to_string_lossy().to_string());
                symlinks.push(format!("{name} -> {target}"));
            } else if metadata.is_dir() {
                dirs.push(format!("{name}/"));
            } else if let Some(line_count) = self.fs_manager.line_count(&entry_path, &metadata) {
                let outline = self
                    .fetch_outline_symbols(&entry_path, parent_id)
                    .await
                    .unwrap_or_default();
                files.push((name, line_count, outline, None));
            } else {
                let size = format_file_size(metadata.len());
                files.push((name, 0, Vec::new(), Some(size)));
            }
        }

        // Sort alphabetically
        dirs.sort();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        symlinks.sort();
        gitignored.sort();

        let mut result = String::new();

        for d in &dirs {
            let _ = writeln!(result, "{d}");
        }

        for (name, line_count, symbols, binary_size) in &files {
            if let Some(size) = binary_size {
                let _ = writeln!(result, "{name}  ({size})");
            } else {
                let _ = writeln!(result, "{name}  ({line_count} lines)");
                for (sym_name, kind, line) in symbols {
                    let kind_str = format_symbol_kind(*kind);
                    let _ = writeln!(result, "  {sym_name} [{kind_str}] L{line}");
                }
            }
        }

        for s in &symlinks {
            let _ = writeln!(result, "{s}");
        }

        if !gitignored.is_empty() {
            let _ = writeln!(result);
            let _ = writeln!(result, "gitignored:");
            for entry in &gitignored {
                let _ = writeln!(result, "  {entry}");
            }
        }

        if result.is_empty() {
            result = "Directory is empty".to_string();
        }

        Ok(result)
    }

    /// Glob pattern match across workspace roots.
    async fn handle_glob_pattern(&self, pattern: &str, parent_id: Option<i64>) -> Result<String> {
        let matcher = Glob::new(pattern)
            .map_err(|e| anyhow!("Invalid glob pattern: {e}"))?
            .compile_matcher();

        let roots = self.client_manager.roots().await;
        let search_roots = if roots.is_empty() {
            vec![std::env::current_dir()?]
        } else {
            roots
        };

        let mut matched_files: Vec<PathBuf> = Vec::new();

        for root in &search_roots {
            let walker = WalkBuilder::new(root)
                .git_ignore(true)
                .hidden(false)
                .build();

            for entry in walker.flatten() {
                let is_file = entry.file_type().is_some_and(|ft| ft.is_file());
                if !is_file {
                    continue;
                }

                let entry_path = entry.path();
                let rel_path = entry_path.strip_prefix(root).unwrap_or(entry_path);

                if matcher.is_match(rel_path) {
                    matched_files.push(entry_path.to_path_buf());
                }
            }
        }

        matched_files.sort();
        matched_files.dedup();

        if matched_files.is_empty() {
            return Ok("No matches found".to_string());
        }

        // Ensure servers exist for any new languages in matched files
        self.client_manager
            .ensure_clients_for_paths(&matched_files)
            .await;

        let mut result = String::new();
        for path in &matched_files {
            let display = path.to_string_lossy();
            let metadata = std::fs::metadata(path).ok();

            if let Some(line_count) = metadata
                .as_ref()
                .and_then(|m| self.fs_manager.line_count(path, m))
            {
                let _ = writeln!(result, "{display}  ({line_count} lines)");
                if let Ok(symbols) = self.fetch_outline_symbols(path, parent_id).await {
                    for (name, kind, line) in &symbols {
                        let kind_str = format_symbol_kind(*kind);
                        let _ = writeln!(result, "  [{kind_str}] {name} L{line}");
                    }
                }
            } else {
                let size = metadata.map_or(0, |m| m.len());
                let _ = writeln!(result, "{display}  ({})", format_file_size(size));
            }
        }

        Ok(result)
    }

    /// Fetches depth-0 outline symbols for a file from LSP.
    ///
    /// Returns `(name, kind_u32, 1-based line)` tuples. On LSP failure,
    /// returns an error (callers typically use `.unwrap_or_default()`).
    async fn fetch_outline_symbols(
        &self,
        path: &Path,
        parent_id: Option<i64>,
    ) -> Result<OutlineSymbols> {
        let (uri_str, client_mutex) = self
            .client_manager
            .ensure_document_open(path, parent_id)
            .await?;

        let mut client = client_mutex.lock().await;
        client.set_parent_id(parent_id);
        let response = client.document_symbols(&uri_str).await?;
        drop(client);

        if response.is_null() {
            return Ok(Vec::new());
        }

        Ok(extract_outline_symbols(&response))
    }
}

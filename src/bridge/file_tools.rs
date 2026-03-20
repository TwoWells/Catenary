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
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::Mutex;

use super::handler::resolve_path;
use super::symbols::{format_symbol_kind, is_outline_kind};
use super::{DocumentManager, DocumentNotification};
use crate::lsp::{ClientManager, LspClient};
use crate::mcp::CallToolResult;

/// Input for the `glob` tool.
#[derive(Debug, Deserialize)]
pub struct GlobInput {
    /// File path, directory path, or glob pattern.
    pub pattern: String,
}

/// Outline symbols for a single file: `(name, kind_u32, 1-based line)`.
type OutlineSymbols = Vec<(String, u32, u32)>;

/// Counts lines in a file.
fn count_lines(path: &Path) -> usize {
    File::open(path)
        .map(|f| BufReader::new(f).lines().count())
        .unwrap_or(0)
}

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
    pub(super) client_manager: Arc<ClientManager>,
    pub(super) doc_manager: Arc<Mutex<DocumentManager>>,
    pub(super) runtime: Handle,
}

impl GlobServer {
    /// Gets the appropriate LSP client for the given file path.
    async fn get_client_for_path(&self, path: &Path) -> Result<Arc<Mutex<LspClient>>> {
        let lang_id = {
            let doc_manager = self.doc_manager.lock().await;
            doc_manager.language_id_for_path(path).to_string()
        };

        self.client_manager
            .get_client_for_path(path, &lang_id)
            .await
    }

    /// Ensures a document is open and synced with the LSP server.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "Client lock held across notification send"
    )]
    async fn ensure_document_open(&self, path: &Path) -> Result<(String, Arc<Mutex<LspClient>>)> {
        let client_mutex = self.get_client_for_path(path).await?;
        let mut doc_manager = self.doc_manager.lock().await;
        let client = client_mutex.lock().await;

        if !client.is_alive() {
            return Err(anyhow!(
                "[{}] server is no longer running",
                client.language()
            ));
        }

        let uri = doc_manager.uri_for_path(path)?;

        if let Some(notification) = doc_manager.ensure_open(path).await? {
            match notification {
                DocumentNotification::Open {
                    language_id,
                    version,
                    text,
                    ..
                } => {
                    client.did_open(&uri, &language_id, version, &text).await?;
                }
                DocumentNotification::Change { version, text, .. } => {
                    client.did_change(&uri, version, &text).await?;
                }
            }

            drop(doc_manager);
            drop(client);
            return Ok((uri, client_mutex.clone()));
        }

        drop(doc_manager);
        drop(client);
        Ok((uri, client_mutex.clone()))
    }

    /// Handles the `glob` tool call.
    pub fn handle_glob(&self, arguments: Option<serde_json::Value>) -> Result<CallToolResult> {
        let input: GlobInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let path = resolve_path(&input.pattern)?;

        tracing::debug!("glob: {}", input.pattern);

        if path.is_file() {
            Ok(self.handle_glob_file(&path))
        } else if path.is_dir() {
            self.handle_glob_dir(&path)
        } else {
            self.handle_glob_pattern(&input.pattern)
        }
    }

    /// File outline: header with line count + depth-0 outline symbols.
    fn handle_glob_file(&self, path: &Path) -> CallToolResult {
        let mut result = String::new();
        let line_count = count_lines(path);
        let display = path.to_string_lossy();
        let _ = writeln!(result, "{display}  ({line_count} lines)");

        if let Ok(symbols) = self.fetch_outline_symbols(path) {
            for (name, kind, line) in &symbols {
                let kind_str = format_symbol_kind(*kind);
                let _ = writeln!(result, "  [{kind_str}] {name} L{line}");
            }
        }

        CallToolResult::text(result)
    }

    /// Directory listing with outline symbols and gitignored section.
    #[allow(clippy::too_many_lines, reason = "Two-pass directory classification")]
    fn handle_glob_dir(&self, dir: &Path) -> Result<CallToolResult> {
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
        let mut files: Vec<(String, usize, OutlineSymbols)> = Vec::new();
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
            } else {
                let line_count = count_lines(&entry_path);
                let outline = self.fetch_outline_symbols(&entry_path).unwrap_or_default();
                files.push((name, line_count, outline));
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

        for (name, line_count, symbols) in &files {
            let _ = writeln!(result, "{name}  ({line_count} lines)");
            for (sym_name, kind, line) in symbols {
                let kind_str = format_symbol_kind(*kind);
                let _ = writeln!(result, "  {sym_name} [{kind_str}] L{line}");
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

        Ok(CallToolResult::text(result))
    }

    /// Glob pattern match across workspace roots.
    fn handle_glob_pattern(&self, pattern: &str) -> Result<CallToolResult> {
        let matcher = Glob::new(pattern)
            .map_err(|e| anyhow!("Invalid glob pattern: {e}"))?
            .compile_matcher();

        let roots = self.runtime.block_on(self.client_manager.roots());
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
                let entry_path = entry.path();
                if !entry_path.is_file() {
                    continue;
                }

                let rel_path = entry_path.strip_prefix(root).unwrap_or(entry_path);

                if matcher.is_match(rel_path) {
                    matched_files.push(entry_path.to_path_buf());
                }
            }
        }

        matched_files.sort();
        matched_files.dedup();

        if matched_files.is_empty() {
            return Ok(CallToolResult::text("No matches found"));
        }

        let mut result = String::new();
        for path in &matched_files {
            let line_count = count_lines(path);
            let display = path.to_string_lossy();
            let _ = writeln!(result, "{display}  ({line_count} lines)");

            if let Ok(symbols) = self.fetch_outline_symbols(path) {
                for (name, kind, line) in &symbols {
                    let kind_str = format_symbol_kind(*kind);
                    let _ = writeln!(result, "  [{kind_str}] {name} L{line}");
                }
            }
        }

        Ok(CallToolResult::text(result))
    }

    /// Fetches depth-0 outline symbols for a file from LSP.
    ///
    /// Returns `(name, kind_u32, 1-based line)` tuples. On LSP failure,
    /// returns an error (callers typically use `.unwrap_or_default()`).
    fn fetch_outline_symbols(&self, path: &Path) -> Result<OutlineSymbols> {
        self.runtime.block_on(async {
            let (uri_str, client_mutex) = self.ensure_document_open(path).await?;

            let response = client_mutex.lock().await.document_symbols(&uri_str).await?;

            if response.is_null() {
                return Ok(Vec::new());
            }

            Ok(extract_outline_symbols(&response))
        })
    }

    /// Extracts a file path from `glob` arguments, returning `Some(path)` only
    /// if the pattern resolves to an existing file. For directories and glob
    /// patterns, returns `None` (triggers wait-for-all-servers).
    pub fn extract_glob_file_path(arguments: Option<&serde_json::Value>) -> Option<PathBuf> {
        let pattern = arguments
            .and_then(|v| v.get("pattern"))
            .and_then(|v| v.as_str())?;
        let path = resolve_path(pattern).ok()?;
        if path.is_file() { Some(path) } else { None }
    }
}

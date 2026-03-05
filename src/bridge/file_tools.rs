// SPDX-License-Identifier: GPL-3.0-or-later
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
use lsp_types::{DocumentSymbolParams, DocumentSymbolResponse, SymbolKind, TextDocumentIdentifier};
use serde::Deserialize;
use std::collections::HashSet;
use std::fmt::Write;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use super::handler::LspBridgeHandler;
use crate::mcp::CallToolResult;

/// Input for the `glob` tool.
#[derive(Debug, Deserialize)]
pub struct GlobInput {
    /// File path, directory path, or glob pattern.
    pub pattern: String,
}

/// Returns `true` for symbol kinds included in outline output.
const fn is_outline_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::STRUCT
            | SymbolKind::CLASS
            | SymbolKind::ENUM
            | SymbolKind::INTERFACE
            | SymbolKind::MODULE
            | SymbolKind::NAMESPACE
            | SymbolKind::PACKAGE
            | SymbolKind::CONSTANT
            | SymbolKind::OBJECT
            | SymbolKind::STRING
            | SymbolKind::KEY
    )
}

/// Outline symbols for a single file: `(name, kind, 1-based line)`.
type OutlineSymbols = Vec<(String, SymbolKind, u32)>;

/// Counts lines in a file.
fn count_lines(path: &Path) -> usize {
    File::open(path)
        .map(|f| BufReader::new(f).lines().count())
        .unwrap_or(0)
}

/// Extracts depth-0 outline symbols from a document symbol response.
fn extract_outline_symbols(response: &DocumentSymbolResponse) -> OutlineSymbols {
    let mut symbols = Vec::new();

    match response {
        DocumentSymbolResponse::Flat(flat) => {
            for sym in flat {
                if is_outline_kind(sym.kind) {
                    symbols.push((
                        sym.name.clone(),
                        sym.kind,
                        sym.location.range.start.line + 1,
                    ));
                }
            }
        }
        DocumentSymbolResponse::Nested(nested) => {
            // Depth 0 only — don't recurse into children
            for sym in nested {
                if is_outline_kind(sym.kind) {
                    symbols.push((sym.name.clone(), sym.kind, sym.range.start.line + 1));
                }
            }
        }
    }

    symbols
}

impl LspBridgeHandler {
    /// Handles the `glob` tool call.
    pub(super) fn handle_glob(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input: GlobInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        let path = Self::resolve_path(&input.pattern)?;

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
                let _ = writeln!(result, "  [{kind:?}] {name} L{line}");
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
                let _ = writeln!(result, "  {sym_name} [{kind:?}] L{line}");
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
                    let _ = writeln!(result, "  [{kind:?}] {name} L{line}");
                }
            }
        }

        Ok(CallToolResult::text(result))
    }

    /// Fetches depth-0 outline symbols for a file from LSP.
    ///
    /// Returns `(name, kind, 1-based line)` tuples. On LSP failure,
    /// returns an error (callers typically use `.unwrap_or_default()`).
    fn fetch_outline_symbols(&self, path: &Path) -> Result<OutlineSymbols> {
        self.runtime.block_on(async {
            let (uri, client_mutex) = self.ensure_document_open(path).await?;

            let params = DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            };

            let response: Option<DocumentSymbolResponse> =
                client_mutex.lock().await.document_symbols(params).await?;

            let Some(response) = response else {
                return Ok(Vec::new());
            };

            Ok(extract_outline_symbols(&response))
        })
    }

    /// Extracts a file path from `glob` arguments, returning `Some(path)` only
    /// if the pattern resolves to an existing file. For directories and glob
    /// patterns, returns `None` (triggers wait-for-all-servers).
    pub(super) fn extract_glob_file_path(arguments: Option<&serde_json::Value>) -> Option<PathBuf> {
        let pattern = arguments
            .and_then(|v| v.get("pattern"))
            .and_then(|v| v.as_str())?;
        let path = Self::resolve_path(pattern).ok()?;
        if path.is_file() { Some(path) } else { None }
    }
}

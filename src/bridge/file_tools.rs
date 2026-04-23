// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Glob tool handler: unified file/directory/pattern browsing.
//!
//! The `glob` tool auto-detects intent from the pattern:
//! - File path → line count header (maps added in 08b)
//! - Directory path → listing with line counts, tiered output
//! - Glob pattern → recursive file tree, tiered output
//!
//! Three tiers with promote-from-bottom selection:
//! - Tier 3: bucketed glob patterns with counts (always fits)
//! - Tier 2: file listing with line counts
//! - Tier 1: file listing with defensive maps (08b)

use anyhow::{Result, anyhow};
use globset::Glob;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::filesystem_manager::{FilesystemManager, format_file_size};
use super::handler::{expand_tilde, resolve_path};
use super::tool_server::ToolServer;
use super::toolbox::ResolvedGlob;
use crate::bucketing::{self, BucketEntry};
use crate::lsp::LspClientManager;

/// Input for the `glob` tool.
#[derive(Debug, Deserialize)]
pub struct GlobInput {
    /// File path, directory path, or glob pattern.
    pub pattern: String,
    /// Symbol path to drill into (stubbed — wired in 08c).
    #[serde(default)]
    pub into: Option<String>,
    /// Glob pattern to exclude from results.
    #[serde(default)]
    pub exclude: Option<String>,
    /// Continuation token from previous result (stubbed — wired in 08b).
    #[serde(default)]
    pub cursor: Option<String>,
    /// Include gitignored files (default: false).
    #[serde(default)]
    pub include_gitignored: bool,
    /// Include hidden/dot files (default: false).
    #[serde(default)]
    pub include_hidden: bool,
}

/// A filesystem entry collected during the glob pipeline.
struct GlobEntry {
    /// Display name (relative to listing root).
    name: String,
    /// True if this is a directory entry.
    is_dir: bool,
    /// Line count for text files (None for dirs and binaries).
    line_count: Option<usize>,
    /// Formatted size for binary files.
    binary_size: Option<String>,
    /// True if this is a symlink.
    is_symlink: bool,
    /// Symlink target path (for display).
    symlink_target: Option<String>,
}

/// A directory node in the tree structure for glob pattern results.
struct DirNode {
    dirs: BTreeMap<String, Self>,
    files: Vec<FileNode>,
}

/// A file leaf in the tree structure.
struct FileNode {
    name: String,
    line_count: Option<usize>,
    binary_size: Option<String>,
}

impl DirNode {
    const fn new() -> Self {
        Self {
            dirs: BTreeMap::new(),
            files: Vec::new(),
        }
    }

    /// Inserts a file at the given path components.
    fn insert(&mut self, components: &[&str], file: FileNode) {
        if components.len() <= 1 {
            self.files.push(file);
        } else {
            let dir = self
                .dirs
                .entry(components[0].to_owned())
                .or_insert_with(Self::new);
            dir.insert(&components[1..], file);
        }
    }

    /// Renders the tree with tab indentation.
    fn render(&self, out: &mut String, depth: usize) {
        let indent: String = "\t".repeat(depth);

        // Directories first, sorted (BTreeMap is sorted).
        for (name, child) in &self.dirs {
            let _ = writeln!(out, "{indent}{name}/");
            child.render(out, depth + 1);
        }

        // Files sorted alphabetically.
        let mut sorted_files: Vec<&FileNode> = self.files.iter().collect();
        sorted_files.sort_by(|a, b| a.name.cmp(&b.name));

        for file in sorted_files {
            if let Some(ref size) = file.binary_size {
                let _ = writeln!(out, "{indent}{}  ({size})", file.name);
            } else if let Some(lc) = file.line_count {
                let _ = writeln!(out, "{indent}{}  ({lc} lines)", file.name);
            } else {
                let _ = writeln!(out, "{indent}{}", file.name);
            }
        }
    }
}

/// Glob tool server: unified file/directory/pattern browsing with tiered output.
pub struct GlobServer {
    pub(super) client_manager: Arc<LspClientManager>,
    pub(super) fs_manager: Arc<FilesystemManager>,
    pub(super) budget: usize,
}

impl ToolServer for GlobServer {
    async fn execute(
        &self,
        params: &serde_json::Value,
        _parent_id: Option<i64>,
    ) -> Result<serde_json::Value> {
        let input: GlobInput = serde_json::from_value(params.clone())
            .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        // Stub: into and cursor not yet implemented.
        if input.into.is_some() {
            return Err(anyhow!("`into` is not yet implemented"));
        }
        if input.cursor.is_some() {
            return Err(anyhow!("`cursor` is not yet implemented"));
        }

        let pattern = expand_tilde(&input.pattern);
        let path = resolve_path(&pattern)?;

        tracing::debug!("glob: {pattern}");

        // Compile exclude pattern if provided. Patterns without a path
        // separator match the basename (like `**/pat`) so the agent can
        // write `exclude="test_*"` instead of `exclude="**/test_*"`.
        let exclude = input
            .exclude
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|pat| {
                let effective = if pat.contains('/') {
                    pat.to_string()
                } else {
                    format!("**/{pat}")
                };
                Glob::new(&effective)
                    .map(|g| g.compile_matcher())
                    .map_err(|e| anyhow!("Invalid exclude pattern: {e}"))
            })
            .transpose()?;

        // Run pipeline.
        let output = if path.is_file() {
            self.handle_glob_file(&path)
        } else if path.is_dir() {
            self.handle_glob_dir(&path, &input, exclude.as_ref())?
        } else {
            self.handle_glob_pattern(&pattern, &input, exclude.as_ref())?
        };

        Ok(Value::String(output))
    }
}

impl GlobServer {
    /// Single file: line count header only. Maps added in 08b.
    fn handle_glob_file(&self, path: &Path) -> String {
        let mut result = String::new();
        let display = path.to_string_lossy();
        let metadata = std::fs::metadata(path).ok();

        if let Some(line_count) = metadata
            .as_ref()
            .and_then(|m| self.fs_manager.line_count(path, m))
        {
            let _ = writeln!(result, "{display}  ({line_count} lines)");
        } else {
            let size = metadata.map_or(0, |m| m.len());
            let _ = writeln!(result, "{display}  ({})", format_file_size(size));
        }

        result
    }

    /// Directory listing with tier selection.
    ///
    /// Collects immediate children, applies visibility and exclude filters,
    /// then selects tier 2 (file listing) or tier 3 (bucketed) based on budget.
    fn handle_glob_dir(
        &self,
        dir: &Path,
        input: &GlobInput,
        exclude: Option<&globset::GlobMatcher>,
    ) -> Result<String> {
        let canonical = dir
            .canonicalize()
            .map_err(|e| anyhow!("Path does not exist: {}: {e}", dir.display()))?;

        let walker = WalkBuilder::new(&canonical)
            .max_depth(Some(1))
            .git_ignore(!input.include_gitignored)
            .hidden(!input.include_hidden)
            .build();

        let mut entries = Vec::new();

        for entry in walker.flatten() {
            let entry_path = entry.into_path();
            if entry_path == canonical {
                continue;
            }

            let name = entry_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            // Apply exclude filter against the entry name.
            if let Some(matcher) = exclude
                && matcher.is_match(&name)
            {
                continue;
            }

            let metadata = entry_path
                .symlink_metadata()
                .map_err(|e| anyhow!("Failed to read metadata for {name}: {e}"))?;

            if metadata.file_type().is_symlink() {
                let target = std::fs::read_link(&entry_path)
                    .map_or_else(|_| "?".to_string(), |t| t.to_string_lossy().to_string());
                // Resolve symlink to get file info from target.
                let resolved_meta = std::fs::metadata(&entry_path).ok();
                let (line_count, binary_size) = self.file_info(&entry_path, resolved_meta.as_ref());
                entries.push(GlobEntry {
                    name,
                    is_dir: resolved_meta
                        .as_ref()
                        .is_some_and(std::fs::Metadata::is_dir),
                    line_count,
                    binary_size,
                    is_symlink: true,
                    symlink_target: Some(target),
                });
            } else if metadata.is_dir() {
                entries.push(GlobEntry {
                    name: format!("{name}/"),
                    is_dir: true,
                    line_count: None,
                    binary_size: None,
                    is_symlink: false,
                    symlink_target: None,
                });
            } else {
                let (line_count, binary_size) = self.file_info(&entry_path, Some(&metadata));
                entries.push(GlobEntry {
                    name,
                    is_dir: false,
                    line_count,
                    binary_size,
                    is_symlink: false,
                    symlink_target: None,
                });
            }
        }

        if entries.is_empty() {
            return Ok("Directory is empty".to_string());
        }

        Ok(select_dir_tier(&entries, self.budget))
    }

    /// Glob pattern match across workspace roots with tree output.
    ///
    /// Absolute patterns (e.g. `/home/user/projects/*`) are searched from
    /// the pattern's base directory rather than workspace roots.
    fn handle_glob_pattern(
        &self,
        pattern: &str,
        input: &GlobInput,
        exclude: Option<&globset::GlobMatcher>,
    ) -> Result<String> {
        let resolved = ResolvedGlob::new(pattern)?;

        let search_roots = if let Some(override_root) = resolved.override_root() {
            vec![override_root.to_path_buf()]
        } else {
            let roots = self.client_manager.roots();
            if roots.is_empty() {
                vec![std::env::current_dir()?]
            } else {
                roots
            }
        };

        let mut matched_files: Vec<(PathBuf, PathBuf)> = Vec::new(); // (abs, root)

        for root in &search_roots {
            let walker = WalkBuilder::new(root)
                .git_ignore(!input.include_gitignored)
                .hidden(!input.include_hidden)
                .build();

            for entry in walker.flatten() {
                let is_file = entry.file_type().is_some_and(|ft| ft.is_file());
                if !is_file {
                    continue;
                }

                let entry_path = entry.path();
                if resolved.is_match(entry_path, root) {
                    // Apply exclude against the relative path.
                    if let Some(matcher) = exclude {
                        let rel = entry_path.strip_prefix(root).unwrap_or(entry_path);
                        if matcher.is_match(rel) {
                            continue;
                        }
                    }
                    matched_files.push((entry_path.to_path_buf(), root.clone()));
                }
            }
        }

        matched_files.sort_by(|a, b| a.0.cmp(&b.0));
        matched_files.dedup_by(|a, b| a.0 == b.0);

        if matched_files.is_empty() {
            return Ok("No matches found".to_string());
        }

        // Build tree structure from matched files.
        let mut root_node = DirNode::new();
        let mut flat_names: Vec<String> = Vec::with_capacity(matched_files.len());

        for (abs_path, root) in &matched_files {
            let rel = abs_path.strip_prefix(root).unwrap_or(abs_path);
            let rel_str = rel.to_string_lossy();
            let components: Vec<&str> = rel_str.split('/').collect();

            let metadata = std::fs::metadata(abs_path).ok();
            let (line_count, binary_size) = self.file_info(abs_path, metadata.as_ref());

            let file_name = components.last().unwrap_or(&"").to_string();
            flat_names.push(file_name.clone());

            root_node.insert(
                &components,
                FileNode {
                    name: file_name,
                    line_count,
                    binary_size,
                },
            );
        }

        // Tier selection: promote from bottom.
        // 1. Tier 3 (bucketed) — always fits.
        let tier3 = render_bucketed(&flat_names, self.budget);

        // 2. Tier 2 (tree listing) — promote if fits.
        let mut tier2 = String::new();
        root_node.render(&mut tier2, 0);
        if tier2.len() <= self.budget {
            return Ok(tier2);
        }

        Ok(tier3)
    }

    /// Extracts file info: `(line_count, binary_size)`.
    fn file_info(
        &self,
        path: &Path,
        metadata: Option<&std::fs::Metadata>,
    ) -> (Option<usize>, Option<String>) {
        metadata.map_or((None, None), |m| {
            self.fs_manager.line_count(path, m).map_or_else(
                || (None, Some(format_file_size(m.len()))),
                |lc| (Some(lc), None),
            )
        })
    }
}

// ─── Tier selection and rendering ───────────────────────────────────────

/// Selects the best tier for a directory listing and renders it.
///
/// Promote-from-bottom:
/// 1. Render tier 3 (bucketed). Always succeeds.
/// 2. Render tier 2 (file listing with line counts). If fits → promote.
/// 3. Tier 1 (maps) stubbed — if tier 2 fits, emit tier 2.
fn select_dir_tier(entries: &[GlobEntry], budget: usize) -> String {
    // Collect file names for bucketing.
    let file_names: Vec<String> = entries
        .iter()
        .filter(|e| !e.is_dir)
        .map(|e| e.name.clone())
        .collect();

    // 1. Tier 3 (bucketed) — always succeeds.
    let tier3 = render_bucketed(&file_names, budget);

    // 2. Tier 2 (file listing, dirs before files).
    let tier2 = render_dir_listing(entries);
    if tier2.len() <= budget {
        return tier2;
    }

    tier3
}

/// Renders a flat directory listing: directories first, then files, sorted.
fn render_dir_listing(entries: &[GlobEntry]) -> String {
    let mut dirs: Vec<&GlobEntry> = entries.iter().filter(|e| e.is_dir).collect();
    let mut files: Vec<&GlobEntry> = entries.iter().filter(|e| !e.is_dir).collect();

    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));

    let mut result = String::new();

    for d in &dirs {
        let _ = writeln!(result, "{}", d.name);
    }

    for f in &files {
        if f.is_symlink {
            let target = f.symlink_target.as_deref().unwrap_or("?");
            if let Some(lc) = f.line_count {
                let _ = writeln!(result, "{} -> {target}  ({lc} lines)", f.name);
            } else if let Some(ref size) = f.binary_size {
                let _ = writeln!(result, "{} -> {target}  ({size})", f.name);
            } else {
                let _ = writeln!(result, "{} -> {target}", f.name);
            }
        } else if let Some(ref size) = f.binary_size {
            let _ = writeln!(result, "{}  ({size})", f.name);
        } else if let Some(lc) = f.line_count {
            let _ = writeln!(result, "{}  ({lc} lines)", f.name);
        } else {
            let _ = writeln!(result, "{}", f.name);
        }
    }

    result
}

/// Renders tier 3 bucketed output from file names.
fn render_bucketed(file_names: &[String], budget: usize) -> String {
    if file_names.is_empty() {
        return String::new();
    }

    let bucket_input: Vec<BucketEntry> = file_names
        .iter()
        .map(|name| BucketEntry {
            value: name.clone(),
            context: None,
        })
        .collect();

    let buckets = bucketing::bucket(&bucket_input, budget, false);

    let mut result = String::new();
    for b in &buckets {
        if b.count == 1 {
            // Single-entry: show full filename.
            if let Some(ref entries) = b.entries {
                if let Some(entry) = entries.first() {
                    let _ = writeln!(result, "{}", entry.value);
                }
            } else {
                let _ = writeln!(result, "{}", b.pattern);
            }
        } else {
            let _ = writeln!(result, "{}  ({} files)", b.pattern, b.count);
        }
    }

    result
}

// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Glob tool handler: unified file/directory/pattern browsing.
//!
//! The `glob` tool auto-detects intent from the pattern:
//! - File path → single file with defensive map (if grammar installed)
//! - Directory path → listing with line counts, maps, and flags
//! - Glob pattern → recursive file tree, tiered output
//!
//! Three tiers with promote-from-bottom selection:
//! - Tier 3: bucketed glob patterns with counts (always fits)
//! - Tier 2: file listing with entry flags (`[symbols available]`, etc.)
//! - Tier 1: file listing with defensive maps from tree-sitter index

use anyhow::{Result, anyhow};
use globset::Glob;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::filesystem_manager::{FilesystemManager, format_file_size};
use super::handler::{expand_tilde, resolve_path};
use super::tool_server::ToolServer;
use super::toolbox::ResolvedGlob;
use crate::bucketing::{self, BucketEntry};
use crate::lsp::LspClientManager;
use crate::ts::{TsIndex, TsSymbol, format_ts_kind};

/// Input for the `glob` tool.
#[derive(Debug, Deserialize)]
pub struct GlobInput {
    /// File path, directory path, or glob pattern.
    pub pattern: String,
    /// Symbol path to drill into (wired in 08c).
    #[serde(default)]
    pub into: Option<String>,
    /// Glob pattern to exclude from results.
    #[serde(default)]
    pub exclude: Option<String>,
    /// Continuation token from previous result.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Include gitignored files (default: false).
    #[serde(default)]
    pub include_gitignored: bool,
    /// Include hidden/dot files (default: false).
    #[serde(default)]
    pub include_hidden: bool,
}

/// A filesystem entry collected during the glob directory pipeline.
#[allow(
    clippy::struct_excessive_bools,
    reason = "flags are independent boolean properties"
)]
struct GlobEntry {
    /// Display name (relative to listing root).
    name: String,
    /// Absolute path for tree-sitter queries.
    abs_path: PathBuf,
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
    /// True if this is a broken symlink (target missing).
    is_broken_symlink: bool,
    /// True if this entry is gitignored (only set when `include_gitignored`).
    is_gitignored: bool,
    /// True if this is a `.catenary_snapshot_*` sidecar file.
    is_snapshot: bool,
}

/// A directory node in the tree structure for glob pattern results.
struct DirNode {
    dirs: BTreeMap<String, Self>,
    files: Vec<FileNode>,
}

/// A file leaf in the tree structure.
struct FileNode {
    name: String,
    abs_path: PathBuf,
    line_count: Option<usize>,
    binary_size: Option<String>,
    is_gitignored: bool,
    is_snapshot: bool,
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

    /// Renders the tree with tab indentation (tier 2: flags, no maps).
    fn render_tier2(
        &self,
        out: &mut String,
        depth: usize,
        ts_index: Option<&TsIndex>,
        maps_threshold: usize,
        maps_deny: &[globset::GlobMatcher],
        fs_manager: &FilesystemManager,
    ) {
        let indent: String = "\t".repeat(depth);

        for (name, child) in &self.dirs {
            let _ = writeln!(out, "{indent}{name}/");
            child.render_tier2(
                out,
                depth + 1,
                ts_index,
                maps_threshold,
                maps_deny,
                fs_manager,
            );
        }

        let mut sorted: Vec<&FileNode> = self.files.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));

        for file in sorted {
            let flags =
                compute_tree_flags(file, ts_index, maps_threshold, maps_deny, fs_manager, false);
            render_file_node(out, file, &indent, &flags);
        }
    }

    /// Renders the tree with tab indentation (tier 1: maps + flags).
    fn render_tier1(
        &self,
        out: &mut String,
        depth: usize,
        outline: &HashMap<PathBuf, Vec<TsSymbol>>,
        children_sets: &HashMap<PathBuf, HashSet<String>>,
        sa_paths: &HashSet<PathBuf>,
    ) {
        let indent: String = "\t".repeat(depth);

        for (name, child) in &self.dirs {
            let _ = writeln!(out, "{indent}{name}/");
            child.render_tier1(out, depth + 1, outline, children_sets, sa_paths);
        }

        let mut sorted: Vec<&FileNode> = self.files.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));

        for file in sorted {
            let has_map = outline.contains_key(&file.abs_path);
            let mut flags = Vec::new();
            if sa_paths.contains(&file.abs_path) && !has_map {
                flags.push("symbols available");
            }
            if file.is_gitignored {
                flags.push("gitignored");
            }
            if file.is_snapshot {
                flags.push("snapshot");
            }
            render_file_node(out, file, &indent, &flags);

            if let Some(syms) = has_map.then(|| outline.get(&file.abs_path)).flatten() {
                let cs = children_sets.get(&file.abs_path);
                let sym_indent = format!("{indent}\t");
                for sym in syms {
                    render_symbol_line(out, sym, cs, &sym_indent);
                }
            }
        }
    }
}

/// Renders a single `FileNode` line with optional flags.
fn render_file_node(out: &mut String, file: &FileNode, indent: &str, flags: &[&str]) {
    let flag_str = if flags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", flags.join(", "))
    };

    if file.is_snapshot {
        let _ = writeln!(out, "{indent}{} [snapshot]", file.name);
    } else if let Some(ref size) = file.binary_size {
        let _ = writeln!(out, "{indent}{}  ({size}){flag_str}", file.name);
    } else if let Some(lc) = file.line_count {
        let _ = writeln!(out, "{indent}{}  ({lc} lines){flag_str}", file.name);
    } else {
        let _ = writeln!(out, "{indent}{}{flag_str}", file.name);
    }
}

/// Computes flags for a `FileNode` in tree rendering (tier 2).
fn compute_tree_flags<'a>(
    file: &FileNode,
    ts_index: Option<&TsIndex>,
    maps_threshold: usize,
    maps_deny: &[globset::GlobMatcher],
    fs_manager: &FilesystemManager,
    map_rendered: bool,
) -> Vec<&'a str> {
    let mut flags = Vec::new();

    if !map_rendered
        && !file.is_snapshot
        && has_grammar_available(&file.abs_path, ts_index)
        && (file.line_count.is_some_and(|lc| lc >= maps_threshold)
            || is_maps_denied(&file.abs_path, maps_deny, fs_manager))
    {
        flags.push("symbols available");
    }

    if file.is_gitignored {
        flags.push("gitignored");
    }
    if file.is_snapshot {
        flags.push("snapshot");
    }

    flags
}

// ─── Glob tool server ─────────────────────────────────────────────────

/// Glob tool server: unified file/directory/pattern browsing with tiered output.
pub struct GlobServer {
    pub(super) client_manager: Arc<LspClientManager>,
    pub(super) fs_manager: Arc<FilesystemManager>,
    pub(super) ts_index: Option<Arc<Mutex<TsIndex>>>,
    pub(super) budget: usize,
    pub(super) maps_threshold: usize,
    pub(super) maps_deny: Vec<globset::GlobMatcher>,
}

impl ToolServer for GlobServer {
    async fn execute(
        &self,
        params: &serde_json::Value,
        _parent_id: Option<i64>,
    ) -> Result<serde_json::Value> {
        let input: GlobInput = serde_json::from_value(params.clone())
            .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        // Stub: into not yet implemented (wired in 08c).
        if input.into.is_some() {
            return Err(anyhow!("`into` is not yet implemented"));
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

        // Decode cursor if provided.
        let after_line = input.cursor.as_deref().map(decode_cursor).transpose()?;

        // Run pipeline.
        let output = if path.is_file() || path.is_symlink() {
            self.handle_glob_file(&path, after_line)
        } else if path.is_dir() {
            self.handle_glob_dir(&path, &input, exclude.as_ref())?
        } else {
            self.handle_glob_pattern(&pattern, &input, exclude.as_ref())?
        };

        Ok(Value::String(output))
    }
}

impl GlobServer {
    /// Single file: header with defensive map (if grammar installed).
    ///
    /// Single files bypass `maps_threshold` — they get a map unless the
    /// grammar is not installed or the path matches `maps_deny`. Pages
    /// when the map exceeds budget.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "guard must live for all index queries"
    )]
    fn handle_glob_file(&self, path: &Path, after_line: Option<u32>) -> String {
        let mut result = String::new();
        let display = path.to_string_lossy();
        let metadata = std::fs::metadata(path).ok();

        // Detect snapshot or broken symlink.
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if is_snapshot(&name) {
            let _ = writeln!(result, "{display} [snapshot]");
            return result;
        }

        // File header with line count or size.
        let line_count = metadata
            .as_ref()
            .and_then(|m| self.fs_manager.line_count(path, m));
        if let Some(lc) = line_count {
            let _ = writeln!(result, "{display}  ({lc} lines)");
        } else {
            let size = metadata.map_or(0, |m| m.len());
            let _ = writeln!(result, "{display}  ({})", format_file_size(size));
        }

        // Single-file map: bypass threshold, check grammar + deny only.
        let Some(ref ts_arc) = self.ts_index else {
            return result;
        };
        let Ok(idx) = ts_arc.lock() else {
            return result;
        };
        if !idx.has_grammar_for(path) || is_maps_denied(path, &self.maps_deny, &self.fs_manager) {
            return result;
        }

        // Ensure fresh and query outline.
        if idx.ensure_fresh(&[path.to_path_buf()]).is_err() {
            return result;
        }
        let Ok(outline) = idx.query_outline_batch(&[path]) else {
            return result;
        };
        let Some(syms) = outline.get(path) else {
            return result;
        };

        // Apply cursor: skip symbols at or before `after_line`.
        let filtered: Vec<&TsSymbol> = syms
            .iter()
            .filter(|s| after_line.is_none_or(|al| s.line > al))
            .collect();

        // Build children set: names that appear as scope for other symbols.
        let children_set = idx
            .query(".*", Some(&[path.to_path_buf()]))
            .ok()
            .map(|all| {
                let mut cs = HashSet::new();
                for (_, s) in &all {
                    if let Some(ref scope) = s.scope {
                        cs.insert(scope.clone());
                    }
                }
                cs
            })
            .unwrap_or_default();

        let mut last_line: Option<u32> = None;
        for sym in &filtered {
            let mut line_buf = String::new();
            render_symbol_line(&mut line_buf, sym, Some(&children_set), "\t");
            if result.len() + line_buf.len() > self.budget {
                // Over budget — emit cursor.
                if let Some(ll) = last_line {
                    let _ = writeln!(result, "[cursor: {}]", encode_cursor(ll));
                }
                return result;
            }
            result.push_str(&line_buf);
            last_line = Some(sym.line);
        }

        result
    }

    /// Directory listing with tier selection.
    ///
    /// Collects immediate children, applies visibility and exclude filters,
    /// detects flags (gitignored, snapshot, broken), then selects tier
    /// based on budget.
    #[allow(clippy::too_many_lines, reason = "sequential pipeline steps")]
    #[allow(
        clippy::significant_drop_tightening,
        reason = "guard must live for all index queries"
    )]
    fn handle_glob_dir(
        &self,
        dir: &Path,
        input: &GlobInput,
        exclude: Option<&globset::GlobMatcher>,
    ) -> Result<String> {
        let canonical = dir
            .canonicalize()
            .map_err(|e| anyhow!("Path does not exist: {}: {e}", dir.display()))?;

        // Build non-gitignored set for flag detection.
        let non_ignored: HashSet<PathBuf> = if input.include_gitignored {
            WalkBuilder::new(&canonical)
                .max_depth(Some(1))
                .git_ignore(true)
                .hidden(!input.include_hidden)
                .build()
                .flatten()
                .map(ignore::DirEntry::into_path)
                .collect()
        } else {
            HashSet::new()
        };

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

            let is_gitignored = input.include_gitignored && !non_ignored.contains(&entry_path);
            let is_snap = is_snapshot(&name);

            let metadata = entry_path
                .symlink_metadata()
                .map_err(|e| anyhow!("Failed to read metadata for {name}: {e}"))?;

            if metadata.file_type().is_symlink() {
                let target = std::fs::read_link(&entry_path)
                    .map_or_else(|_| "?".to_string(), |t| t.to_string_lossy().to_string());
                let resolved_meta = std::fs::metadata(&entry_path).ok();
                let is_broken = resolved_meta.is_none();

                let (line_count, binary_size) = if is_broken || is_snap {
                    (None, None)
                } else {
                    self.file_info(&entry_path, resolved_meta.as_ref())
                };

                entries.push(GlobEntry {
                    name,
                    abs_path: entry_path,
                    is_dir: resolved_meta
                        .as_ref()
                        .is_some_and(std::fs::Metadata::is_dir),
                    line_count,
                    binary_size,
                    is_symlink: true,
                    symlink_target: Some(target),
                    is_broken_symlink: is_broken,
                    is_gitignored,
                    is_snapshot: is_snap,
                });
            } else if metadata.is_dir() {
                entries.push(GlobEntry {
                    name: format!("{name}/"),
                    abs_path: entry_path,
                    is_dir: true,
                    line_count: None,
                    binary_size: None,
                    is_symlink: false,
                    symlink_target: None,
                    is_broken_symlink: false,
                    is_gitignored,
                    is_snapshot: false,
                });
            } else {
                let (line_count, binary_size) = if is_snap {
                    (None, None)
                } else {
                    self.file_info(&entry_path, Some(&metadata))
                };
                entries.push(GlobEntry {
                    name,
                    abs_path: entry_path,
                    is_dir: false,
                    line_count,
                    binary_size,
                    is_symlink: false,
                    symlink_target: None,
                    is_broken_symlink: false,
                    is_gitignored,
                    is_snapshot: is_snap,
                });
            }
        }

        if entries.is_empty() {
            return Ok("Directory is empty".to_string());
        }

        let ts_guard = self.ts_index.as_ref().and_then(|m| m.lock().ok());
        Ok(select_dir_tier(
            &entries,
            self.budget,
            ts_guard.as_deref(),
            self.maps_threshold,
            &self.maps_deny,
            &self.fs_manager,
        ))
    }

    /// Glob pattern match across workspace roots with tree output.
    ///
    /// Absolute patterns (e.g. `/home/user/projects/*`) are searched from
    /// the pattern's base directory rather than workspace roots.
    #[allow(clippy::too_many_lines, reason = "sequential pipeline steps")]
    #[allow(
        clippy::significant_drop_tightening,
        reason = "guard must live for all index queries"
    )]
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

        // Build non-gitignored set for flag detection.
        let non_ignored: HashSet<PathBuf> = if input.include_gitignored {
            let mut set = HashSet::new();
            for root in &search_roots {
                let walker = WalkBuilder::new(root)
                    .git_ignore(true)
                    .hidden(!input.include_hidden)
                    .build();
                set.extend(
                    walker
                        .flatten()
                        .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
                        .map(ignore::DirEntry::into_path),
                );
            }
            set
        } else {
            HashSet::new()
        };

        let mut matched_files: Vec<(PathBuf, PathBuf, bool)> = Vec::new(); // (abs, root, gitignored)

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
                    if let Some(matcher) = exclude
                        && matcher.is_match(entry_path.strip_prefix(root).unwrap_or(entry_path))
                    {
                        continue;
                    }
                    let gitignored = input.include_gitignored && !non_ignored.contains(entry_path);
                    matched_files.push((entry_path.to_path_buf(), root.clone(), gitignored));
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

        for (abs_path, root, gitignored) in &matched_files {
            let rel = abs_path.strip_prefix(root).unwrap_or(abs_path);
            let rel_str = rel.to_string_lossy();
            let components: Vec<&str> = rel_str.split('/').collect();

            let metadata = std::fs::metadata(abs_path).ok();
            let file_name = components.last().unwrap_or(&"").to_string();
            let snap = is_snapshot(&file_name);

            let (line_count, binary_size) = if snap {
                (None, None)
            } else {
                self.file_info(abs_path, metadata.as_ref())
            };

            flat_names.push(file_name.clone());

            root_node.insert(
                &components,
                FileNode {
                    name: file_name,
                    abs_path: abs_path.clone(),
                    line_count,
                    binary_size,
                    is_gitignored: *gitignored,
                    is_snapshot: snap,
                },
            );
        }

        // Tier selection: promote from bottom.
        let ts_guard = self.ts_index.as_ref().and_then(|m| m.lock().ok());
        let ts_ref = ts_guard.as_deref();

        // 1. Tier 3 (bucketed) — always fits.
        let tier3 = render_bucketed(&flat_names, self.budget);

        // 2. Tier 2 (tree listing with flags) — promote if fits.
        let mut tier2 = String::new();
        root_node.render_tier2(
            &mut tier2,
            0,
            ts_ref,
            self.maps_threshold,
            &self.maps_deny,
            &self.fs_manager,
        );
        if tier2.len() > self.budget {
            return Ok(tier3);
        }

        // 3. Tier 1 (tree listing with maps) — promote if eligible.
        if let Some(idx) = ts_ref {
            let abs_paths: Vec<PathBuf> = matched_files.iter().map(|(p, _, _)| p.clone()).collect();
            let eligible: Vec<&Path> = abs_paths
                .iter()
                .filter(|p| {
                    is_map_eligible(
                        p,
                        &matched_files,
                        self.maps_threshold,
                        &self.maps_deny,
                        idx,
                        &self.fs_manager,
                    )
                })
                .map(PathBuf::as_path)
                .collect();

            if !eligible.is_empty() {
                let _ =
                    idx.ensure_fresh(&eligible.iter().map(|p| p.to_path_buf()).collect::<Vec<_>>());
                if let Ok(outline) = idx.query_outline_batch(&eligible)
                    && !outline.is_empty()
                {
                    let children_sets = build_children_sets(idx, &eligible);
                    let sa_paths = build_sa_paths(&matched_files, idx);

                    let mut tier1 = String::new();
                    root_node.render_tier1(&mut tier1, 0, &outline, &children_sets, &sa_paths);
                    if tier1.len() <= self.budget {
                        return Ok(tier1);
                    }
                }
            }
        }

        Ok(tier2)
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

// ─── Map eligibility ──────────────────────────────────────────────────

/// Returns `true` if a file is eligible for defensive maps in a directory listing.
fn is_map_eligible_entry(
    entry: &GlobEntry,
    maps_threshold: usize,
    maps_deny: &[globset::GlobMatcher],
    ts_index: &TsIndex,
    fs_manager: &FilesystemManager,
) -> bool {
    !entry.is_dir
        && !entry.is_broken_symlink
        && !entry.is_snapshot
        && entry.line_count.is_some_and(|lc| lc >= maps_threshold)
        && ts_index.has_grammar_for(&entry.abs_path)
        && !is_maps_denied(&entry.abs_path, maps_deny, fs_manager)
}

/// Returns `true` if a matched file in a glob pattern tree is map-eligible.
fn is_map_eligible(
    path: &Path,
    _matched_files: &[(PathBuf, PathBuf, bool)],
    maps_threshold: usize,
    maps_deny: &[globset::GlobMatcher],
    ts_index: &TsIndex,
    fs_manager: &FilesystemManager,
) -> bool {
    let metadata = std::fs::metadata(path).ok();
    let line_count = metadata
        .as_ref()
        .and_then(|m| fs_manager.line_count(path, m));

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    !is_snapshot(&name)
        && line_count.is_some_and(|lc| lc >= maps_threshold)
        && ts_index.has_grammar_for(path)
        && !is_maps_denied(path, maps_deny, fs_manager)
}

/// Returns `true` if a grammar is installed for the file's language.
fn has_grammar_available(path: &Path, ts_index: Option<&TsIndex>) -> bool {
    ts_index.is_some_and(|idx| idx.has_grammar_for(path))
}

/// Returns `true` if the file matches any `maps_deny` pattern.
fn is_maps_denied(
    abs_path: &Path,
    maps_deny: &[globset::GlobMatcher],
    fs_manager: &FilesystemManager,
) -> bool {
    if maps_deny.is_empty() {
        return false;
    }
    let rel = fs_manager
        .resolve_root(abs_path)
        .and_then(|root| abs_path.strip_prefix(&root).ok().map(Path::to_path_buf))
        .unwrap_or_else(|| abs_path.to_path_buf());
    maps_deny.iter().any(|pat| pat.is_match(&rel))
}

/// Returns `true` if the filename matches the snapshot sidecar pattern.
fn is_snapshot(name: &str) -> bool {
    name.contains(".catenary_snapshot_")
}

// ─── Symbol rendering ─────────────────────────────────────────────────

/// Renders a single symbol line: `:start-end <Kind> Name[/]`.
fn render_symbol_line(
    out: &mut String,
    sym: &TsSymbol,
    children_set: Option<&HashSet<String>>,
    indent: &str,
) {
    let kind_label = format_ts_kind(&sym.kind);
    let trailing = if children_set.is_some_and(|cs| cs.contains(&sym.name)) {
        "/"
    } else {
        ""
    };
    let _ = writeln!(
        out,
        "{indent}:{}-{} <{kind_label}> {}{trailing}",
        sym.line + 1,
        sym.end_line + 1,
        sym.name,
    );
}

// ─── Structure deduplication ──────────────────────────────────────────

/// Fingerprint: sorted `(kind, name)` pairs as a single string key.
fn make_fingerprint(syms: &[TsSymbol]) -> String {
    let mut pairs: Vec<(&str, &str)> = syms
        .iter()
        .map(|s| (s.kind.as_str(), s.name.as_str()))
        .collect();
    pairs.sort_unstable();
    pairs
        .iter()
        .map(|(k, n)| format!("{k}\x00{n}"))
        .collect::<Vec<_>>()
        .join("\x01")
}

/// A bounding symbol for a dedup group.
struct BoundingSymbol {
    name: String,
    kind: String,
    min_line: u32,
    max_end_line: u32,
    has_children: bool,
}

/// A shared dedup group: (entry indices, bounding symbols).
type SharedGroup = (Vec<usize>, Vec<BoundingSymbol>);

/// Computes structure dedup groups for map-eligible directory entries.
///
/// Returns `(shared_groups, individual_indices)` where shared groups
/// are entries with identical fingerprints (≥2 files) and individuals
/// are unique.
fn compute_dedup(
    eligible_indices: &[usize],
    entries: &[GlobEntry],
    outline: &HashMap<PathBuf, Vec<TsSymbol>>,
    ts_index: &TsIndex,
) -> (Vec<SharedGroup>, Vec<usize>) {
    // Group by fingerprint.
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for &idx in eligible_indices {
        if let Some(syms) = outline.get(&entries[idx].abs_path) {
            let fp = make_fingerprint(syms);
            groups.entry(fp).or_default().push(idx);
        }
    }

    let mut shared = Vec::new();
    let mut individual = Vec::new();

    for (_fp, indices) in groups {
        if indices.len() == 1 {
            individual.push(indices[0]);
        } else {
            // Compute bounding ranges using the first file as representative.
            let rep = &entries[indices[0]];
            let rep_syms = outline.get(&rep.abs_path);
            let bounding = rep_syms.map_or_else(Vec::new, |syms| {
                syms.iter()
                    .map(|sym| {
                        let mut min_l = sym.line;
                        let mut max_e = sym.end_line;
                        for &other_idx in &indices[1..] {
                            if let Some(other_syms) = outline.get(&entries[other_idx].abs_path) {
                                for s in other_syms {
                                    if s.kind == sym.kind && s.name == sym.name {
                                        min_l = min_l.min(s.line);
                                        max_e = max_e.max(s.end_line);
                                    }
                                }
                            }
                        }
                        BoundingSymbol {
                            name: sym.name.clone(),
                            kind: sym.kind.clone(),
                            min_line: min_l,
                            max_end_line: max_e,
                            has_children: ts_index.has_children(&rep.abs_path, &sym.name),
                        }
                    })
                    .collect()
            });
            shared.push((indices, bounding));
        }
    }

    (shared, individual)
}

// ─── Tier selection and rendering ─────────────────────────────────────

/// Selects the best tier for a directory listing and renders it.
///
/// Promote-from-bottom:
/// 1. Render tier 3 (bucketed). Always succeeds.
/// 2. Render tier 2 (file listing with flags). If fits → promote.
/// 3. Render tier 1 (file listing with defensive maps). If fits → promote.
fn select_dir_tier(
    entries: &[GlobEntry],
    budget: usize,
    ts_index: Option<&TsIndex>,
    maps_threshold: usize,
    maps_deny: &[globset::GlobMatcher],
    fs_manager: &FilesystemManager,
) -> String {
    // Collect file names for bucketing.
    let file_names: Vec<String> = entries
        .iter()
        .filter(|e| !e.is_dir)
        .map(|e| e.name.clone())
        .collect();

    // 1. Tier 3 (bucketed) — always succeeds.
    let tier3 = render_bucketed(&file_names, budget);

    // 2. Tier 2 (file listing with flags).
    let tier2 =
        render_dir_listing_with_flags(entries, ts_index, maps_threshold, maps_deny, fs_manager);
    if tier2.len() > budget {
        return tier3;
    }

    // 3. Tier 1 (file listing with maps).
    let Some(idx) = ts_index else {
        return tier2;
    };

    let eligible_indices: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| is_map_eligible_entry(e, maps_threshold, maps_deny, idx, fs_manager))
        .map(|(i, _)| i)
        .collect();

    if eligible_indices.is_empty() {
        return tier2;
    }

    // Ensure fresh for all eligible files.
    let fresh_paths: Vec<PathBuf> = eligible_indices
        .iter()
        .map(|&i| entries[i].abs_path.clone())
        .collect();
    let _ = idx.ensure_fresh(&fresh_paths);

    // Query outline symbols.
    let eligible_refs: Vec<&Path> = fresh_paths.iter().map(PathBuf::as_path).collect();
    let Ok(outline) = idx.query_outline_batch(&eligible_refs) else {
        return tier2;
    };

    if outline.is_empty() {
        return tier2;
    }

    // Build children sets and render tier 1.
    let children_sets = build_children_sets(idx, &eligible_refs);
    let tier1 = render_dir_listing_with_maps(
        entries,
        &eligible_indices,
        &outline,
        &children_sets,
        idx,
        ts_index,
        maps_deny,
        fs_manager,
    );

    if tier1.len() <= budget { tier1 } else { tier2 }
}

/// Renders a flat directory listing with entry flags (tier 2).
fn render_dir_listing_with_flags(
    entries: &[GlobEntry],
    ts_index: Option<&TsIndex>,
    maps_threshold: usize,
    maps_deny: &[globset::GlobMatcher],
    fs_manager: &FilesystemManager,
) -> String {
    let mut dirs: Vec<&GlobEntry> = entries.iter().filter(|e| e.is_dir).collect();
    let mut files: Vec<&GlobEntry> = entries.iter().filter(|e| !e.is_dir).collect();

    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));

    let mut result = String::new();

    for d in &dirs {
        let _ = writeln!(result, "{}", d.name);
    }

    for f in &files {
        let flags = compute_entry_flags(f, ts_index, maps_threshold, maps_deny, fs_manager, false);
        render_entry_line(&mut result, f, &flags);
    }

    result
}

/// Renders a flat directory listing with defensive maps (tier 1).
#[allow(
    clippy::too_many_arguments,
    reason = "tier 1 rendering needs full context"
)]
fn render_dir_listing_with_maps(
    entries: &[GlobEntry],
    eligible_indices: &[usize],
    outline: &HashMap<PathBuf, Vec<TsSymbol>>,
    children_sets: &HashMap<PathBuf, HashSet<String>>,
    ts_index: &TsIndex,
    ts_opt: Option<&TsIndex>,
    maps_deny: &[globset::GlobMatcher],
    fs_manager: &FilesystemManager,
) -> String {
    let mut dirs: Vec<&GlobEntry> = entries.iter().filter(|e| e.is_dir).collect();
    dirs.sort_by(|a, b| a.name.cmp(&b.name));

    let mut files: Vec<(usize, &GlobEntry)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| !e.is_dir)
        .collect();
    files.sort_by(|a, b| a.1.name.cmp(&b.1.name));

    // Structure deduplication.
    let (shared_groups, individual_indices) =
        compute_dedup(eligible_indices, entries, outline, ts_index);

    // Build lookup: entry index → group index (for shared groups).
    let mut entry_to_group: HashMap<usize, usize> = HashMap::new();
    for (gi, (indices, _)) in shared_groups.iter().enumerate() {
        for &idx in indices {
            entry_to_group.insert(idx, gi);
        }
    }

    let individual_set: HashSet<usize> = individual_indices.iter().copied().collect();
    let mut rendered_groups: HashSet<usize> = HashSet::new();

    let mut result = String::new();

    for d in &dirs {
        let _ = writeln!(result, "{}", d.name);
    }

    for &(idx, f) in &files {
        // Check if this file is part of a shared dedup group.
        if let Some(&gi) = entry_to_group.get(&idx) {
            if rendered_groups.contains(&gi) {
                continue; // Already rendered with the group.
            }
            rendered_groups.insert(gi);

            let (group_indices, bounding) = &shared_groups[gi];
            // Render group: list files, then shared map.
            for &gidx in group_indices {
                let gf = &entries[gidx];
                let flags = compute_entry_flags(gf, ts_opt, 0, maps_deny, fs_manager, true);
                render_entry_line(&mut result, gf, &flags);
            }
            let _ = writeln!(result, "common structure (ranges are bounding):");
            for sym in bounding {
                let trailing = if sym.has_children { "/" } else { "" };
                let kind_label = format_ts_kind(&sym.kind);
                let _ = writeln!(
                    result,
                    "\t:{}-{} <{kind_label}> {}{trailing}",
                    sym.min_line + 1,
                    sym.max_end_line + 1,
                    sym.name,
                );
            }
        } else if individual_set.contains(&idx) {
            // Individual map.
            let flags = compute_entry_flags(f, ts_opt, 0, maps_deny, fs_manager, true);
            render_entry_line(&mut result, f, &flags);
            if let Some(syms) = outline.get(&f.abs_path) {
                let cs = children_sets.get(&f.abs_path);
                for sym in syms {
                    render_symbol_line(&mut result, sym, cs, "\t");
                }
            }
        } else {
            // Non-eligible file: render with flags (may have [symbols available]).
            let has_sa = has_grammar_available(&f.abs_path, ts_opt)
                && is_maps_denied(&f.abs_path, maps_deny, fs_manager);
            let mut flags = Vec::new();
            if has_sa {
                flags.push("symbols available");
            }
            if f.is_gitignored {
                flags.push("gitignored");
            }
            if f.is_snapshot {
                flags.push("snapshot");
            }
            if f.is_broken_symlink {
                flags.push("broken");
            }
            render_entry_line_raw(&mut result, f, &flags);
        }
    }

    result
}

/// Computes entry flags for a `GlobEntry`.
fn compute_entry_flags<'a>(
    entry: &GlobEntry,
    ts_index: Option<&TsIndex>,
    maps_threshold: usize,
    maps_deny: &[globset::GlobMatcher],
    fs_manager: &FilesystemManager,
    map_rendered: bool,
) -> Vec<&'a str> {
    let mut flags = Vec::new();

    if entry.is_broken_symlink {
        flags.push("broken");
        return flags;
    }

    if entry.is_snapshot {
        flags.push("snapshot");
        return flags;
    }

    if !map_rendered
        && has_grammar_available(&entry.abs_path, ts_index)
        && entry.line_count.is_some_and(|lc| lc >= maps_threshold)
    {
        flags.push("symbols available");
    }

    if map_rendered
        && has_grammar_available(&entry.abs_path, ts_index)
        && is_maps_denied(&entry.abs_path, maps_deny, fs_manager)
    {
        flags.push("symbols available");
    }

    if entry.is_gitignored {
        flags.push("gitignored");
    }

    flags
}

/// Renders a `GlobEntry` line with flags.
fn render_entry_line(out: &mut String, entry: &GlobEntry, flags: &[&str]) {
    let flag_str = if flags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", flags.join(", "))
    };

    if entry.is_broken_symlink {
        let target = entry.symlink_target.as_deref().unwrap_or("?");
        let _ = writeln!(out, "{} -> {target} [broken]", entry.name);
    } else if entry.is_snapshot {
        let _ = writeln!(out, "{} [snapshot]", entry.name);
    } else if entry.is_symlink {
        let target = entry.symlink_target.as_deref().unwrap_or("?");
        if let Some(lc) = entry.line_count {
            let _ = writeln!(out, "{} -> {target}  ({lc} lines){flag_str}", entry.name);
        } else if let Some(ref size) = entry.binary_size {
            let _ = writeln!(out, "{} -> {target}  ({size}){flag_str}", entry.name);
        } else {
            let _ = writeln!(out, "{} -> {target}{flag_str}", entry.name);
        }
    } else if let Some(ref size) = entry.binary_size {
        let _ = writeln!(out, "{}  ({size}){flag_str}", entry.name);
    } else if let Some(lc) = entry.line_count {
        let _ = writeln!(out, "{}  ({lc} lines){flag_str}", entry.name);
    } else {
        let _ = writeln!(out, "{}{flag_str}", entry.name);
    }
}

/// Renders a `GlobEntry` line with explicit flags (no flag computation).
fn render_entry_line_raw(out: &mut String, entry: &GlobEntry, flags: &[&str]) {
    render_entry_line(out, entry, flags);
}

// ─── Bucketed rendering (tier 3) ──────────────────────────────────────

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

// ─── Cursor-based paging ──────────────────────────────────────────────

/// Encodes a cursor token from a 0-based line number.
fn encode_cursor(line: u32) -> String {
    format!("g{line}")
}

/// Decodes a cursor token to a 0-based line number.
fn decode_cursor(token: &str) -> Result<u32> {
    token
        .strip_prefix('g')
        .ok_or_else(|| anyhow!("invalid cursor token"))
        .and_then(|s| {
            s.parse::<u32>()
                .map_err(|_| anyhow!("invalid cursor token"))
        })
}

// ─── Helpers ──────────────────────────────────────────────────────────

/// Builds per-file children sets from the tree-sitter index.
///
/// For each file, collects the set of symbol names that are used as
/// `scope` by other symbols — these are containers that get trailing `/`.
fn build_children_sets(ts_index: &TsIndex, files: &[&Path]) -> HashMap<PathBuf, HashSet<String>> {
    let mut result = HashMap::new();
    for &path in files {
        let mut cs = HashSet::new();
        if let Ok(all) = ts_index.query(".*", Some(&[path.to_path_buf()])) {
            for (_, s) in &all {
                if let Some(ref scope) = s.scope {
                    cs.insert(scope.clone());
                }
            }
        }
        if !cs.is_empty() {
            result.insert(path.to_path_buf(), cs);
        }
    }
    result
}

/// Builds the set of paths that have grammars available (for `[symbols available]`).
fn build_sa_paths(
    matched_files: &[(PathBuf, PathBuf, bool)],
    ts_index: &TsIndex,
) -> HashSet<PathBuf> {
    matched_files
        .iter()
        .filter(|(p, _, _)| ts_index.has_grammar_for(p))
        .map(|(p, _, _)| p.clone())
        .collect()
}

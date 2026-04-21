// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Grep tool: ripgrep + tree-sitter index pipeline with LSP enrichment.

use super::toolbox::ResolvedGlob;
use anyhow::{Result, anyhow};
use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{Searcher, Sink, SinkMatch};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::debug;

use super::filesystem_manager::FilesystemManager;
use super::handler::{check_server_health, display_path};
use super::tool_server::ToolServer;
use crate::lsp::LspClientManager;
use crate::ts::{TsIndex, TsSymbol, format_ts_kind};

/// Input for grep tool.
#[derive(Debug, Deserialize)]
pub struct GrepInput {
    /// Search pattern (supports `|` for alternation, passed to ripgrep).
    pub pattern: String,
    /// Glob pattern to scope the search (optional).
    #[serde(default)]
    pub glob: Option<String>,
    /// Glob pattern to exclude from matches (optional).
    #[serde(default)]
    pub exclude: Option<String>,
    /// Include gitignored files (default: false).
    #[serde(default)]
    pub include_gitignored: bool,
    /// Include hidden/dot files (default: false).
    #[serde(default)]
    pub include_hidden: bool,
}

/// A classified hit from the grep pipeline.
struct GrepHit {
    file: PathBuf,
    line: u32,
    matched_text: String,
    classification: HitClass,
}

/// Classification of a ripgrep hit against the tree-sitter index.
enum HitClass {
    /// rg hit at a tree-sitter definition line.
    Symbol { symbol: TsSymbol },
    /// rg hit at a non-definition line, with optional enclosing structure.
    Reference { enclosing: Option<TsSymbol> },
    /// Symbol identified via `prepareRename` (no-grammar path).
    PrepareRenameSymbol,
    /// Keyword filtered out via `prepareRename` (will be dropped).
    Keyword,
}

/// Grep tool server: ripgrep + tree-sitter index pipeline with LSP enrichment.
pub struct GrepServer {
    pub(super) client_manager: Arc<LspClientManager>,
    pub(super) fs_manager: Arc<FilesystemManager>,
    pub(super) notified_offline: Arc<std::sync::Mutex<HashSet<String>>>,
    pub(super) ts_index: Option<Arc<std::sync::Mutex<TsIndex>>>,
}

impl ToolServer for GrepServer {
    async fn execute(
        &self,
        params: &serde_json::Value,
        parent_id: Option<i64>,
    ) -> Result<serde_json::Value> {
        let input: GrepInput = serde_json::from_value(params.clone())
            .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        if input.pattern.is_empty() {
            return Err(anyhow!("pattern must be non-empty"));
        }

        // Wait for all servers ready (grep doesn't target a specific file).
        // Track dead languages so the pipeline can skip prepareRename for them.
        let mut dead_languages: HashSet<String> = HashSet::new();
        let clients = self.client_manager.clients().await;
        for (key, client_mutex) in &clients {
            if !client_mutex.lock().await.wait_ready().await {
                debug!(
                    "[{}] server died \u{2014} tool will run in degraded mode",
                    key.language_id
                );
                dead_languages.insert(key.language_id.clone());
            }
        }

        // Emit state-transition notifications.
        let touched: Vec<String> = clients.keys().map(|k| k.language_id.clone()).collect();
        check_server_health(&self.client_manager, &touched, &self.notified_offline).await;

        // Split top-level alternation into independent arms
        let arms = split_alternation(&input.pattern);

        let mut all_output = String::new();
        for arm in &arms {
            let arm_input = GrepInput {
                pattern: arm.clone(),
                glob: input.glob.clone(),
                exclude: input.exclude.clone(),
                include_gitignored: input.include_gitignored,
                include_hidden: input.include_hidden,
            };
            let output = self.run(arm_input, parent_id, &dead_languages).await?;
            if !output.is_empty() {
                if !all_output.is_empty() {
                    all_output.push('\n');
                }
                all_output.push_str(&output);
            }
        }

        if all_output.is_empty() {
            return Ok(Value::String("No results found".to_string()));
        }

        Ok(Value::String(all_output))
    }
}

impl GrepServer {
    /// Grep pipeline: ripgrep + tree-sitter index + hit classification.
    #[allow(clippy::too_many_lines, reason = "Core grep orchestration")]
    async fn run(
        &self,
        input: GrepInput,
        parent_id: Option<i64>,
        dead_languages: &HashSet<String>,
    ) -> Result<String> {
        use std::fmt::Write;

        debug!("Grep request: pattern={}", input.pattern);

        let resolved_glob = input
            .glob
            .as_deref()
            .map(ResolvedGlob::new)
            .transpose()?
            .map(Arc::new);
        let resolved_exclude = input
            .exclude
            .as_deref()
            .map(ResolvedGlob::new)
            .transpose()?
            .map(Arc::new);

        // Determine effective search roots: absolute glob overrides workspace roots.
        let workspace_roots = self.client_manager.roots();
        let effective_roots = if let Some(ref rg) = resolved_glob
            && let Some(override_root) = rg.override_root()
        {
            vec![override_root.to_path_buf()]
        } else {
            workspace_roots
        };

        // Step 1: Ripgrep scoped to file set → raw hits with matched text.
        let rg = Self::ripgrep_matches(
            &input.pattern,
            &effective_roots,
            resolved_glob.as_ref(),
            resolved_exclude.as_ref(),
            input.include_gitignored,
            input.include_hidden,
            &self.fs_manager,
        )?;

        if rg.file_lines.is_empty() {
            return Ok(String::new());
        }

        // Step 2: Ensure servers exist for matched files.
        let rg_paths: Vec<PathBuf> = rg.file_lines.keys().map(PathBuf::from).collect();
        self.client_manager
            .ensure_clients_for_paths(&rg_paths)
            .await;

        // Step 3: Tree-sitter index freshness check and query.
        let (ts_symbols, grammar_files) = if let Some(ref index_mutex) = self.ts_index {
            let index = index_mutex
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let re_pattern = format!("(?i){}", &input.pattern);
            if let Err(e) = index.ensure_fresh(&rg_paths) {
                debug!("tree-sitter freshness check failed: {e}");
            }
            // query() and find_enclosing() use throwaway read connections,
            // so we only hold the lock for ensure_fresh (writes).
            let ts_syms = index
                .query(&re_pattern, Some(&rg_paths))
                .unwrap_or_default();
            let gf: HashSet<String> = rg_paths
                .iter()
                .filter(|p| index.has_grammar_for(p))
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            drop(index);
            (ts_syms, gf)
        } else {
            (Vec::new(), HashSet::new())
        };

        // Build lookup: (file_path, line) → TsSymbol for definitions
        let mut def_lookup: HashMap<(String, u32), TsSymbol> = HashMap::new();
        for (path, sym) in &ts_symbols {
            let path_str = path.to_string_lossy().to_string();
            def_lookup.insert(
                (path_str, sym.line),
                TsSymbol {
                    name: sym.name.clone(),
                    kind: sym.kind.clone(),
                    line: sym.line,
                    end_line: sym.end_line,
                    scope: sym.scope.clone(),
                    scope_kind: sym.scope_kind.clone(),
                },
            );
        }

        // Step 4: Classify each rg hit.
        let mut hits: Vec<GrepHit> = Vec::new();

        for (file_str, line_map) in &rg.file_line_texts {
            let file_path = PathBuf::from(file_str);
            let has_grammar = grammar_files.contains(file_str);

            for (&line_1, texts) in line_map {
                let line_0 = line_1 - 1;
                let matched_text = texts.first().map(|(t, _)| t.clone()).unwrap_or_default();

                if has_grammar {
                    // Check if this line is a definition
                    if let Some(sym) = def_lookup.get(&(file_str.clone(), line_0)) {
                        hits.push(GrepHit {
                            file: file_path.clone(),
                            line: line_0,
                            matched_text: matched_text.clone(),
                            classification: HitClass::Symbol {
                                symbol: TsSymbol {
                                    name: sym.name.clone(),
                                    kind: sym.kind.clone(),
                                    line: sym.line,
                                    end_line: sym.end_line,
                                    scope: sym.scope.clone(),
                                    scope_kind: sym.scope_kind.clone(),
                                },
                            },
                        });
                    } else {
                        // Non-definition line — find enclosing structure via SQL.
                        // find_enclosing opens a throwaway read connection internally.
                        let enclosing = self.ts_index.as_ref().and_then(|idx| {
                            idx.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .find_enclosing(&file_path, line_0)
                                .ok()
                                .flatten()
                        });
                        hits.push(GrepHit {
                            file: file_path.clone(),
                            line: line_0,
                            matched_text,
                            classification: HitClass::Reference { enclosing },
                        });
                    }
                } else {
                    // No grammar — check if the language server is alive
                    let lang = self.fs_manager.language_id(&file_path);
                    let server_dead = lang
                        .as_ref()
                        .is_some_and(|l| dead_languages.contains(l.as_str()));

                    if server_dead {
                        // Server unavailable — emit bare reference, skip LSP
                        hits.push(GrepHit {
                            file: file_path.clone(),
                            line: line_0,
                            matched_text,
                            classification: HitClass::Reference { enclosing: None },
                        });
                    } else {
                        // Server alive — use prepareRename for keyword discrimination
                        let col = texts.first().map_or(0, |(_, c)| *c);
                        let is_symbol = self
                            .prepare_rename_check(&file_path, line_0, col, parent_id)
                            .await;
                        if is_symbol {
                            hits.push(GrepHit {
                                file: file_path.clone(),
                                line: line_0,
                                matched_text,
                                classification: HitClass::PrepareRenameSymbol,
                            });
                        } else {
                            hits.push(GrepHit {
                                file: file_path.clone(),
                                line: line_0,
                                matched_text,
                                classification: HitClass::Keyword,
                            });
                        }
                    }
                }
            }
        }

        // Drop keywords
        hits.retain(|h| !matches!(h.classification, HitClass::Keyword));

        if hits.is_empty() {
            return Ok(String::new());
        }

        // Temporary flat output (replaced by 06b with proper tiers)
        let mut output = String::new();

        // Group by matched_text for stable output
        let mut by_name: BTreeMap<String, Vec<&GrepHit>> = BTreeMap::new();
        for hit in &hits {
            let key = match &hit.classification {
                HitClass::Symbol { symbol } => symbol.name.clone(),
                _ => hit.matched_text.clone(),
            };
            by_name.entry(key).or_default().push(hit);
        }

        for (name, group) in &by_name {
            if !output.is_empty() {
                output.push('\n');
            }

            for hit in group {
                let path = display_path(&hit.file.to_string_lossy(), &self.fs_manager);
                let line_1 = hit.line + 1;

                match &hit.classification {
                    HitClass::Symbol { symbol } => {
                        let kind = format_ts_kind(&symbol.kind);
                        let scope_prefix = symbol
                            .scope
                            .as_ref()
                            .zip(symbol.scope_kind.as_ref())
                            .map_or_else(String::new, |(sn, sk)| {
                                format!("<{}> {}/", format_ts_kind(sk), sn)
                            });
                        let _ = writeln!(output, "{scope_prefix}<{kind}> {name}  {path}:{line_1}");
                    }
                    HitClass::Reference { enclosing } => {
                        if let Some(enc) = enclosing {
                            let enc_kind = format_ts_kind(&enc.kind);
                            let enc_span = if enc.line == enc.end_line {
                                format!(":{}", enc.line + 1)
                            } else {
                                format!(":{}-{}", enc.line + 1, enc.end_line + 1)
                            };
                            let _ = writeln!(
                                output,
                                "{path}:{line_1} (ref in <{enc_kind}> {}{})",
                                enc.name, enc_span
                            );
                        } else {
                            let _ = writeln!(output, "{path}:{line_1}");
                        }
                    }
                    HitClass::PrepareRenameSymbol => {
                        let _ = writeln!(output, "{name}  {path}:{line_1}");
                    }
                    HitClass::Keyword => {} // already filtered
                }
            }
        }

        let trimmed_len = output.trim_end().len();
        output.truncate(trimmed_len);
        Ok(output)
    }

    /// Checks `prepareRename` at a position to distinguish symbols from keywords.
    ///
    /// Returns `true` if the position is a symbol, `false` if keyword.
    /// Callers should check server health before calling — this method
    /// returns `false` if the server is unreachable.
    async fn prepare_rename_check(
        &self,
        path: &Path,
        line_0: u32,
        col: u32,
        parent_id: Option<i64>,
    ) -> bool {
        let Ok((uri_str, client_mutex)) = self
            .client_manager
            .ensure_document_open(path, parent_id)
            .await
        else {
            return false;
        };

        let mut client = client_mutex.lock().await;
        client.set_parent_id(parent_id);
        let result = if client.supports_rename() {
            match client.prepare_rename(&uri_str, line_0, col).await {
                Ok(v) if v.is_null() => false, // null → keyword
                _ => true,                     // range or error → assume symbol
            }
        } else {
            // No renameProvider — can't distinguish, assume symbol
            true
        };
        drop(client);
        self.client_manager
            .close_document(&uri_str, &client_mutex)
            .await;
        result
    }

    /// Searches workspace roots for pattern matches using the `grep-*` crates
    /// (ripgrep's internals). Walks files in parallel and returns matched
    /// strings and per-file line numbers in a single pass per file.
    ///
    /// # Errors
    ///
    /// Returns an error if the pattern is not a valid regex.
    fn ripgrep_matches(
        pattern: &str,
        roots: &[PathBuf],
        glob: Option<&Arc<ResolvedGlob>>,
        exclude: Option<&Arc<ResolvedGlob>>,
        include_gitignored: bool,
        include_hidden: bool,
        fs_manager: &Arc<FilesystemManager>,
    ) -> Result<RipgrepMatches> {
        use ignore::WalkState;
        use std::sync::Mutex as StdMutex;

        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(true)
            .build(pattern)
            .map_err(|e| anyhow!("Invalid regex pattern: {e}"))?;

        let collected = Arc::new(StdMutex::new(Vec::<ThreadMatches>::new()));

        // WalkBuilder flags use "skip" semantics: .hidden(true) = skip hidden
        let skip_gitignored = !include_gitignored;
        let skip_hidden = !include_hidden;

        for root in roots {
            let walker = WalkBuilder::new(root)
                .git_ignore(skip_gitignored)
                .hidden(skip_hidden)
                .build_parallel();

            walker.run(|| {
                let matcher = matcher.clone();
                let glob = glob.cloned();
                let exclude = exclude.cloned();
                let root = root.clone();
                let fs_manager = Arc::clone(fs_manager);
                let mut state = CollectOnDrop {
                    local: ThreadMatches::default(),
                    collected: Arc::clone(&collected),
                };

                Box::new(move |entry| {
                    let Ok(entry) = entry else {
                        return WalkState::Continue;
                    };
                    let path = entry.path();
                    if !path.is_file() {
                        return WalkState::Continue;
                    }

                    if let Some(rg) = &glob
                        && !rg.is_match(path, &root)
                    {
                        return WalkState::Continue;
                    }
                    if let Some(rg) = &exclude
                        && rg.is_match(path, &root)
                    {
                        return WalkState::Continue;
                    }

                    // Skip binary files — no meaningful text matches
                    if let Ok(metadata) = path.metadata()
                        && fs_manager.is_binary(path, &metadata)
                    {
                        return WalkState::Continue;
                    }

                    let path_str = path.to_string_lossy().to_string();
                    let mut sink = MatchSink {
                        matcher: &matcher,
                        path: &path_str,
                        local: &mut state.local,
                    };

                    if let Err(e) = Searcher::new().search_path(&matcher, path, &mut sink) {
                        debug!("grep: skipping {path_str}: {e}");
                    }

                    WalkState::Continue
                })
            });
        }

        let parts = Arc::into_inner(collected)
            .ok_or_else(|| anyhow!("walker threads still hold references"))?
            .into_inner()
            .map_err(|e| anyhow!("lock poisoned: {e}"))?;

        Ok(RipgrepMatches::merge(parts))
    }
}

/// Wrapper that pushes per-thread match data into a shared collector on drop.
/// Each parallel walker thread owns one of these; when `run()` returns and the
/// closures are dropped, each thread's accumulated matches are flushed.
struct CollectOnDrop {
    local: ThreadMatches,
    collected: Arc<std::sync::Mutex<Vec<ThreadMatches>>>,
}

impl Drop for CollectOnDrop {
    fn drop(&mut self) {
        let local = std::mem::take(&mut self.local);
        if local.file_lines.is_empty() {
            return;
        }
        if let Ok(mut vec) = self.collected.lock() {
            vec.push(local);
        }
    }
}

/// Collects per-file match data for the ripgrep library search.
struct MatchSink<'a> {
    matcher: &'a grep_regex::RegexMatcher,
    path: &'a str,
    local: &'a mut ThreadMatches,
}

impl Sink for MatchSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let Some(line_num) = mat
            .line_number()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n > 0)
        else {
            return Ok(true);
        };

        let line_bytes = mat.bytes();

        // Extract each individual match from the line (--only-matching equivalent)
        let mut at = 0;
        while at < line_bytes.len() {
            let Ok(Some(m)) = self.matcher.find_at(line_bytes, at) else {
                break;
            };
            if m.start() == m.end() {
                // Zero-width match — advance to avoid infinite loop
                at = m.end() + 1;
                continue;
            }
            if let Ok(text) = std::str::from_utf8(&line_bytes[m]) {
                let col = u32::try_from(m.start()).unwrap_or(0);
                self.local
                    .file_line_texts
                    .entry(self.path.to_string())
                    .or_default()
                    .entry(line_num)
                    .or_default()
                    .push((text.to_string(), col));
            }
            at = m.end();
        }

        self.local
            .file_lines
            .entry(self.path.to_string())
            .or_default()
            .push(line_num);

        Ok(true)
    }
}

// ─── Alternation splitting ────────────────────────────────────────────

/// Result of a ripgrep `--only-matching` search.
#[derive(Default)]
struct RipgrepMatches {
    /// Per-file line numbers (for heatmap tier).
    file_lines: BTreeMap<String, Vec<u32>>,
    /// Per-file, per-line matched texts with column offsets
    /// `(matched_text, column_byte_offset)` for hit classification
    /// and for no-grammar `prepareRename` positions.
    file_line_texts: HashMap<String, HashMap<u32, Vec<(String, u32)>>>,
}

impl RipgrepMatches {
    /// Merges per-thread match accumulators into a single result.
    fn merge(parts: Vec<ThreadMatches>) -> Self {
        let mut file_lines: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        let mut file_line_texts: HashMap<String, HashMap<u32, Vec<(String, u32)>>> = HashMap::new();

        for part in parts {
            for (file, lines) in part.file_lines {
                file_lines.entry(file).or_default().extend(lines);
            }
            for (file, line_map) in part.file_line_texts {
                let entry = file_line_texts.entry(file).or_default();
                for (line, texts) in line_map {
                    entry.entry(line).or_default().extend(texts);
                }
            }
        }

        Self {
            file_lines,
            file_line_texts,
        }
    }
}

/// Per-thread match accumulator used during parallel file walking.
#[derive(Default)]
struct ThreadMatches {
    /// Per-file line numbers.
    file_lines: BTreeMap<String, Vec<u32>>,
    /// Per-file, per-line matched texts with column offsets.
    file_line_texts: HashMap<String, HashMap<u32, Vec<(String, u32)>>>,
}

/// Splits a regex pattern on top-level `|` alternation.
///
/// Only splits on `|` when depth == 0 and not inside a character class.
/// `foo|bar` → `["foo", "bar"]`. `(foo|bar)_baz` → `["(foo|bar)_baz"]`.
fn split_alternation(pattern: &str) -> Vec<String> {
    let mut arms = Vec::new();
    let mut depth: usize = 0;
    let mut in_class = false;
    let mut start = 0;
    let mut escaped = false;

    for (i, ch) in pattern.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if in_class {
            if ch == ']' {
                in_class = false;
            }
            continue;
        }
        match ch {
            '[' => in_class = true,
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '|' if depth == 0 => {
                arms.push(pattern[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    arms.push(pattern[start..].to_string());
    arms.retain(|a| !a.is_empty());
    if arms.is_empty() {
        arms.push(pattern.to_string());
    }
    arms
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    // ─── display_path tests ──────────────────────────────────────────────

    #[test]
    fn test_display_path_strips_root() {
        let fs = FilesystemManager::new();
        fs.set_roots(vec![PathBuf::from("/home/user/project")]);
        assert_eq!(
            display_path("/home/user/project/src/main.rs", &fs),
            "src/main.rs"
        );
    }

    #[test]
    fn test_display_path_no_matching_root() {
        let fs = FilesystemManager::new();
        fs.set_roots(vec![PathBuf::from("/home/user/project")]);
        assert_eq!(
            display_path("/other/path/file.rs", &fs),
            "/other/path/file.rs"
        );
    }

    // ─── split_alternation tests ─────────────────────────────────────────

    #[test]
    fn test_split_top_level() {
        assert_eq!(split_alternation("foo|bar"), vec!["foo", "bar"]);
    }

    #[test]
    fn test_split_nested_no_split() {
        assert_eq!(split_alternation("(foo|bar)_baz"), vec!["(foo|bar)_baz"]);
    }

    #[test]
    fn test_split_character_class() {
        assert_eq!(split_alternation("[a|b]_c"), vec!["[a|b]_c"]);
    }

    #[test]
    fn test_split_three_arms() {
        assert_eq!(split_alternation("a|b|c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_split_no_alternation() {
        assert_eq!(split_alternation("foobar"), vec!["foobar"]);
    }

    #[test]
    fn test_split_escaped_pipe() {
        assert_eq!(split_alternation(r"foo\|bar"), vec![r"foo\|bar"]);
    }
}

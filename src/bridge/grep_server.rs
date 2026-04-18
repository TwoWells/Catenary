// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Grep tool: ripgrep + workspace/symbol pipeline with LSP enrichment.

use super::toolbox::ResolvedGlob;
use anyhow::{Result, anyhow};
use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{Searcher, Sink, SinkMatch};
use ignore::WalkBuilder;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::debug;

use super::filesystem_manager::FilesystemManager;
use super::handler::{check_server_health, display_path};
use super::symbols::{
    self, SymbolInfo, extract_locations, extract_symbol_infos, format_symbol_kind,
};
use super::tool_server::ToolServer;
use crate::lsp::{LspClient, LspClientManager};

/// Maximum unique LSP symbols for hover display in output. Above this
/// threshold, hover content is omitted but structural enrichment (references,
/// callers, implementations, subtypes) is always included. Also caps the
/// bootstrap discovery loop.
const GREP_HOVER_THRESHOLD: usize = 10;

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

/// Grep tool server: ripgrep + workspace/symbol pipeline with LSP enrichment.
pub struct GrepServer {
    pub(super) client_manager: Arc<LspClientManager>,
    pub(super) fs_manager: Arc<FilesystemManager>,
    pub(super) notified_offline: Arc<std::sync::Mutex<HashSet<String>>>,
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

        // Wait for all servers ready (grep doesn't target a specific file)
        let clients = self.client_manager.clients().await;
        for (lang, client_mutex) in &clients {
            if !client_mutex.lock().await.wait_ready().await {
                debug!("[{lang}] server died \u{2014} tool will run in degraded mode");
            }
        }

        // Emit state-transition notifications.
        let touched: Vec<String> = clients.keys().cloned().collect();
        check_server_health(&self.client_manager, &touched, &self.notified_offline).await;

        // Run pipeline
        let output = self.run(input, parent_id).await?;

        Ok(Value::String(output))
    }
}

impl GrepServer {
    /// Grep: ripgrep + `workspace/symbol("")` pipeline with LSP enrichment.
    #[allow(clippy::too_many_lines, reason = "Core grep orchestration")]
    async fn run(&self, input: GrepInput, parent_id: Option<i64>) -> Result<String> {
        use std::fmt::Write;

        let re = Regex::new(&format!("(?i){}", &input.pattern))
            .map_err(|e| anyhow!("Invalid regex pattern: {e}"))?;

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
        let workspace_roots = self.client_manager.roots().await;
        let effective_roots = if let Some(ref rg) = resolved_glob
            && let Some(override_root) = rg.override_root()
        {
            vec![override_root.to_path_buf()]
        } else {
            workspace_roots
        };

        // 1. Ripgrep: get matched strings + file/line heatmap in one pass
        let rg = Self::ripgrep_matches(
            &input.pattern,
            &effective_roots,
            resolved_glob.as_ref(),
            resolved_exclude.as_ref(),
            input.include_gitignored,
            input.include_hidden,
            &self.fs_manager,
        )?;

        // 1b. Ensure servers exist for any new languages in matched files
        let rg_paths: Vec<PathBuf> = rg.file_lines.keys().map(PathBuf::from).collect();
        self.client_manager
            .ensure_clients_for_paths(&rg_paths)
            .await;

        // 2. Symbol universe: workspace/symbol("") + regex filter, with rg fallback
        let mut symbols = {
            let clients = self.client_manager.clients().await;

            // Try workspace/symbol("") first — returns the full symbol index
            let mut all_symbols: Vec<SymbolInfo> =
                self.fetch_symbol_universe(&clients, parent_id).await;

            // Fallback: if symbol("") returned nothing, use rg matched strings
            if all_symbols.is_empty() && !rg.matched_strings.is_empty() {
                all_symbols = self
                    .fetch_symbols_by_queries(&rg.matched_strings, &clients, parent_id)
                    .await;
            }

            // Regex filter against the user's pattern
            all_symbols.retain(|s| re.is_match(&s.name));

            // Dedupe by (name, file_path, line)
            let mut seen: HashSet<(String, String, u32)> = HashSet::new();
            all_symbols.retain(|s| seen.insert((s.name.clone(), s.file_path.clone(), s.line)));

            all_symbols
        };

        // Filter symbols to glob/exclude scope
        if let Some(ref rg) = resolved_glob {
            symbols.retain(|s| {
                effective_roots
                    .iter()
                    .any(|root| rg.is_match(Path::new(&s.file_path), root))
            });
        }
        if let Some(ref rg) = resolved_exclude {
            symbols.retain(|s| {
                !effective_roots
                    .iter()
                    .any(|root| rg.is_match(Path::new(&s.file_path), root))
            });
        }

        let show_hover = symbols.len() <= GREP_HOVER_THRESHOLD;

        // 3. Group symbols by name (preserving order of first occurrence)
        let mut name_order: Vec<String> = Vec::new();
        let mut by_name: BTreeMap<String, Vec<&SymbolInfo>> = BTreeMap::new();
        for sym in &symbols {
            if !by_name.contains_key(&sym.name) {
                name_order.push(sym.name.clone());
            }
            by_name.entry(sym.name.clone()).or_default().push(sym);
        }

        // 4. Always enrich: references, callers, implementations, subtypes
        let mut enrichments: HashMap<(String, u32), SymbolEnrichment> = HashMap::new();
        let mut all_ref_lines: HashMap<String, HashSet<u32>> = HashMap::new();

        for sym in &symbols {
            let enrichment = self.enrich_symbol(sym, parent_id).await;
            for (file, lines) in &enrichment.ref_lines {
                all_ref_lines.entry(file.clone()).or_default().extend(lines);
            }
            let key = (sym.file_path.clone(), sym.line);
            enrichments.insert(key, enrichment);
        }

        // 4b. Rg-bootstrapped enrichment: enrich unaccounted rg hits via hover
        let br = self
            .bootstrap_from_rg(&rg, &by_name, &all_ref_lines, parent_id)
            .await;
        enrichments.extend(br.enrichments);
        for (file, lines) in br.ref_lines {
            all_ref_lines.entry(file).or_default().extend(lines);
        }
        for n in &br.name_order {
            if !name_order.contains(n) {
                name_order.push(n.clone());
            }
        }
        let bootstrapped = br.symbols;
        for sym in &bootstrapped {
            by_name.entry(sym.name.clone()).or_default().push(sym);
        }

        // 5. Assign rg lines to symbol names; leftover lines are non-code hits
        let rg_by_name = assign_rg_lines_to_symbols(&by_name, &all_ref_lines, &rg);

        // 6. Build unified name_order: LSP symbol names + rg-only matched strings
        // Add rg-only headings (matched strings that don't correspond to any LSP symbol)
        for heading in rg_by_name.keys() {
            if !heading.is_empty() && !name_order.contains(heading) {
                name_order.push(heading.clone());
            }
        }

        if name_order.is_empty() && rg.file_lines.is_empty() {
            return Ok("No results found".to_string());
        }

        let mut output = String::new();

        // Single loop over all headings (LSP symbols and rg-only)
        for name in &name_order {
            if !output.is_empty() {
                output.push('\n');
            }
            let _ = writeln!(output, "# {name}");

            // Non-code rg hits for this heading, with hover-resolved context
            if let Some(lines) = rg_by_name.get(name.as_str())
                && !lines.is_empty()
            {
                let _ = writeln!(output);
                for (file, file_lines) in lines {
                    let path = display_path(file, &self.fs_manager);
                    let _ = writeln!(output, "{path} {}", format_line_ranges(file_lines));
                }
            }

            // Definition sub-headings (only for LSP symbols)
            if let Some(defs) = by_name.get(name) {
                for sym in defs {
                    let kind = format_symbol_kind(sym.kind);
                    let path = display_path(&sym.file_path, &self.fs_manager);
                    let line = sym.line + 1;
                    let _ = writeln!(output, "\n## [{kind}] {path}:{line}");

                    let key = (sym.file_path.clone(), sym.line);
                    if let Some(enrichment) = enrichments.get(&key) {
                        // Hover only when symbol count is within threshold
                        if show_hover && let Some(hover) = &enrichment.hover {
                            output.push('\n');
                            for line in hover.lines() {
                                let _ = writeln!(output, "> {line}");
                            }
                        }

                        // Structural enrichment always shown
                        let mut labeled_lines: HashSet<(String, u32)> = HashSet::new();

                        if !enrichment.incoming_calls.is_empty() {
                            let _ = writeln!(output, "\n### Callers\n");
                            for (name, file, line) in &enrichment.incoming_calls {
                                let path = display_path(file, &self.fs_manager);
                                let _ = writeln!(output, "{name}  {path}:{line}");
                                labeled_lines.insert((file.clone(), *line));
                            }
                        }

                        if !enrichment.implementations.is_empty() {
                            let _ = writeln!(output, "\n### Implementations\n");
                            for (file, line) in &enrichment.implementations {
                                let path = display_path(file, &self.fs_manager);
                                let _ = writeln!(output, "{path}:{line}");
                                labeled_lines.insert((file.clone(), *line));
                            }
                        }

                        if !enrichment.subtypes.is_empty() {
                            let _ = writeln!(output, "\n### Subtypes\n");
                            for (name, file, line) in &enrichment.subtypes {
                                let path = display_path(file, &self.fs_manager);
                                let _ = writeln!(output, "{name}  {path}:{line}");
                                labeled_lines.insert((file.clone(), *line));
                            }
                        }

                        // References (excluding definition line and labeled lines)
                        let ref_output = format_symbol_references(
                            enrichment,
                            &sym.file_path,
                            sym.line,
                            &self.fs_manager,
                            &labeled_lines,
                        );
                        if !ref_output.is_empty() {
                            let _ = writeln!(output, "\n### References\n\n{ref_output}");
                        }
                    }
                }
            }
        }

        // Trim trailing whitespace
        let trimmed_len = output.trim_end().len();
        output.truncate(trimmed_len);

        if output.is_empty() {
            return Ok("No results found".to_string());
        }

        Ok(output)
    }

    /// Fetches the full symbol universe via `workspace/symbol("")` from all servers.
    /// Resolves URI-only symbols when the server supports `workspaceSymbol/resolve`.
    async fn fetch_symbol_universe(
        &self,
        clients: &HashMap<String, Arc<Mutex<LspClient>>>,
        parent_id: Option<i64>,
    ) -> Vec<SymbolInfo> {
        let mut all_symbols: Vec<SymbolInfo> = Vec::new();

        for client_mutex in clients.values() {
            let mut client = client_mutex.lock().await;
            client.set_parent_id(parent_id);
            let supports_resolve = client.supports_workspace_symbol_resolve();

            let Ok(response) = client.workspace_symbols("").await else {
                continue;
            };

            // Extract symbols that have full location info
            all_symbols.extend(extract_symbol_infos(&response));

            // Resolve URI-only symbols when server supports it
            if supports_resolve && let Some(arr) = response.as_array() {
                for item in arr {
                    let has_uri = item.get("location").and_then(|l| l.get("uri")).is_some();
                    let has_range = item.get("location").and_then(|l| l.get("range")).is_some();
                    if has_uri
                        && !has_range
                        && let Ok(resolved) = client.workspace_symbol_resolve(item).await
                    {
                        all_symbols.extend(extract_symbol_infos(&Value::Array(vec![resolved])));
                    }
                }
            }
        }

        all_symbols
    }

    /// Fallback: queries workspace/symbol with each matched string (ticket 08 behavior).
    /// Resolves URI-only symbols via resolve when supported.
    async fn fetch_symbols_by_queries(
        &self,
        queries: &[String],
        clients: &HashMap<String, Arc<Mutex<LspClient>>>,
        parent_id: Option<i64>,
    ) -> Vec<SymbolInfo> {
        let mut all_symbols: Vec<SymbolInfo> = Vec::new();

        for query in queries {
            for client_mutex in clients.values() {
                let mut client = client_mutex.lock().await;
                client.set_parent_id(parent_id);
                let supports_resolve = client.supports_workspace_symbol_resolve();

                let Ok(response) = client.workspace_symbols(query).await else {
                    continue;
                };

                all_symbols.extend(extract_symbol_infos(&response));

                if supports_resolve && let Some(arr) = response.as_array() {
                    for item in arr {
                        let has_uri = item.get("location").and_then(|l| l.get("uri")).is_some();
                        let has_range = item.get("location").and_then(|l| l.get("range")).is_some();
                        if has_uri
                            && !has_range
                            && let Ok(resolved) = client.workspace_symbol_resolve(item).await
                        {
                            all_symbols.extend(extract_symbol_infos(&Value::Array(vec![resolved])));
                        }
                    }
                }
            }
        }

        all_symbols
    }

    /// Enriches a symbol with hover, references, and kind-specific labels.
    async fn enrich_symbol(&self, sym: &SymbolInfo, parent_id: Option<i64>) -> SymbolEnrichment {
        let path = PathBuf::from(&sym.file_path);
        self.enrich_at_position(&path, sym.line, sym.character, sym.kind, parent_id)
            .await
    }

    /// Enriches a position with hover, references, and kind-specific labels.
    #[allow(clippy::too_many_lines, reason = "Sequential LSP calls by kind")]
    async fn enrich_at_position(
        &self,
        path: &Path,
        line_0: u32,
        col: u32,
        kind: u32,
        parent_id: Option<i64>,
    ) -> SymbolEnrichment {
        let mut enrichment = SymbolEnrichment::default();

        let Ok((uri_str, client_mutex)) = self
            .client_manager
            .ensure_document_open(path, parent_id)
            .await
        else {
            return enrichment;
        };

        let mut client = client_mutex.lock().await;
        client.set_parent_id(parent_id);

        // Hover — signature + docs
        if let Ok(hover) = client.hover(&uri_str, line_0, col).await {
            enrichment.hover = extract_hover_text_from_value(&hover);
        }

        // References — collect line numbers for dedup
        if let Ok(refs) = client.references(&uri_str, line_0, col, true).await {
            for (file, line, _char) in extract_locations(&refs) {
                enrichment
                    .ref_lines
                    .entry(file)
                    .or_default()
                    .insert(line + 1);
            }
        }

        // Kind-specific enrichment
        match kind {
            // Functions/methods/constructors → incoming calls
            symbols::SK_FUNCTION | symbols::SK_METHOD | symbols::SK_CONSTRUCTOR => {
                if let Ok(response) = client.prepare_call_hierarchy(&uri_str, line_0, col).await
                    && let Some(items) = response.as_array()
                {
                    for item in items {
                        if let Ok(calls) = client.incoming_calls(item).await {
                            extract_incoming_calls(&calls, &mut enrichment);
                        }
                    }
                }
            }

            // Structs/classes/enums → implementations
            symbols::SK_STRUCT | symbols::SK_CLASS | symbols::SK_ENUM => {
                if let Ok(response) = client.implementation(&uri_str, line_0, col).await {
                    for (file, line, _char) in extract_locations(&response) {
                        enrichment.implementations.push((file, line + 1));
                    }
                }
            }

            // Interfaces/traits → subtypes
            symbols::SK_INTERFACE => {
                if let Ok(response) = client.prepare_type_hierarchy(&uri_str, line_0, col).await
                    && let Some(items) = response.as_array()
                {
                    for item in items {
                        if let Ok(subs) = client.subtypes(item).await {
                            extract_subtypes(&subs, &mut enrichment);
                        }
                    }
                }
            }

            _ => {}
        }

        drop(client);
        self.client_manager
            .close_document(&uri_str, &client_mutex)
            .await;

        enrichment
    }

    /// Enriches a position with kind inference — tries all kind-specific
    /// enrichments and infers kind from what returns results. Used by the
    /// rg-bootstrapped enrichment path where kind is unknown.
    #[allow(
        clippy::too_many_lines,
        reason = "Sequential LSP calls for kind inference"
    )]
    async fn enrich_at_position_infer_kind(
        &self,
        path: &Path,
        line_0: u32,
        col: u32,
        parent_id: Option<i64>,
    ) -> (u32, Option<String>, SymbolEnrichment) {
        let mut enrichment = SymbolEnrichment::default();
        let mut inferred_kind = symbols::SK_VARIABLE;
        let mut resolved_name: Option<String> = None;

        let Ok((uri_str, client_mutex)) = self
            .client_manager
            .ensure_document_open(path, parent_id)
            .await
        else {
            return (inferred_kind, resolved_name, enrichment);
        };

        let mut client = client_mutex.lock().await;
        client.set_parent_id(parent_id);

        // Hover — signature + docs
        if let Ok(hover) = client.hover(&uri_str, line_0, col).await {
            enrichment.hover = extract_hover_text_from_value(&hover);
        }

        // References — collect line numbers for dedup
        if let Ok(refs) = client.references(&uri_str, line_0, col, true).await {
            for (file, line, _char) in extract_locations(&refs) {
                enrichment
                    .ref_lines
                    .entry(file)
                    .or_default()
                    .insert(line + 1);
            }
        }

        // Try all kind-specific enrichments — infer kind from results

        // Call hierarchy → FUNCTION
        if let Ok(response) = client.prepare_call_hierarchy(&uri_str, line_0, col).await
            && let Some(items) = response.as_array()
            && !items.is_empty()
        {
            inferred_kind = symbols::SK_FUNCTION;
            // The first item's name is the LSP-canonical symbol name.
            resolved_name = items
                .first()
                .and_then(|i| i.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string);
            for item in items {
                if let Ok(calls) = client.incoming_calls(item).await {
                    extract_incoming_calls(&calls, &mut enrichment);
                }
            }
        }

        // Implementation → STRUCT (only if not already identified as function)
        if inferred_kind == symbols::SK_VARIABLE
            && let Ok(response) = client.implementation(&uri_str, line_0, col).await
        {
            let locs = extract_locations(&response);
            if !locs.is_empty() {
                inferred_kind = symbols::SK_STRUCT;
                for (file, line, _char) in locs {
                    enrichment.implementations.push((file, line + 1));
                }
            }
        }

        // Type hierarchy → INTERFACE (only if not already identified)
        if inferred_kind == symbols::SK_VARIABLE
            && let Ok(response) = client.prepare_type_hierarchy(&uri_str, line_0, col).await
            && let Some(items) = response.as_array()
            && !items.is_empty()
        {
            inferred_kind = symbols::SK_INTERFACE;
            resolved_name = items
                .first()
                .and_then(|i| i.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string);
            for item in items {
                if let Ok(subs) = client.subtypes(item).await {
                    extract_subtypes(&subs, &mut enrichment);
                }
            }
        }

        drop(client);
        self.client_manager
            .close_document(&uri_str, &client_mutex)
            .await;

        (inferred_kind, resolved_name, enrichment)
    }

    /// Bootstraps LSP enrichment from unaccounted rg hit positions.
    ///
    /// After the symbol universe enrichment, some rg hits may not be explained
    /// by any universe symbol (truncation, keyword prefixes). This method
    /// uses `prepareRename` to distinguish symbols from keywords, then
    /// enriches confirmed symbols with full LSP queries.
    ///
    /// Returns owned `SymbolInfo` values (plus enrichments and ref lines).
    /// The caller merges these into `by_name` where both the universe
    /// `symbols` vec and the returned vec are in scope.
    #[allow(clippy::too_many_lines, reason = "Iterative elimination loop")]
    async fn bootstrap_from_rg(
        &self,
        rg: &RipgrepMatches,
        by_name: &BTreeMap<String, Vec<&SymbolInfo>>,
        all_ref_lines: &HashMap<String, HashSet<u32>>,
        parent_id: Option<i64>,
    ) -> BootstrapResult {
        let mut result = BootstrapResult {
            symbols: Vec::new(),
            enrichments: HashMap::new(),
            ref_lines: HashMap::new(),
            name_order: Vec::new(),
        };

        // Build accounted set: definition lines + reference lines from universe
        let mut accounted: HashMap<String, HashSet<u32>> = HashMap::new();
        for defs in by_name.values() {
            for sym in defs {
                accounted
                    .entry(sym.file_path.clone())
                    .or_default()
                    .insert(sym.line + 1);
            }
        }
        for (file, ref_set) in all_ref_lines {
            accounted.entry(file.clone()).or_default().extend(ref_set);
        }

        // Collect unaccounted rg hits: (file, line_1based, matched_text, col)
        let mut unaccounted: Vec<(String, u32, String, u32)> = Vec::new();
        for (file, line_map) in &rg.file_line_texts {
            for (&line, texts) in line_map {
                if accounted.get(file).is_some_and(|s| s.contains(&line)) {
                    continue;
                }
                for (text, col) in texts {
                    unaccounted.push((file.clone(), line, text.clone(), *col));
                }
            }
        }

        if unaccounted.is_empty() {
            return result;
        }

        // Identifier token regex for extracting symbols from matched text
        let Ok(ident_re) = Regex::new(r"[a-zA-Z_]\w*") else {
            return result;
        };

        // Track names we've already bootstrapped to avoid duplicates
        let mut bootstrapped_names: HashSet<String> = HashSet::new();

        // Total distinct symbol count: universe + bootstrapped
        let total_symbols = |result: &BootstrapResult| by_name.len() + result.symbols.len();

        for (file, line_1, matched_text, match_col) in &unaccounted {
            if total_symbols(&result) >= GREP_HOVER_THRESHOLD {
                break;
            }

            // Check if this line is now accounted (by a prior bootstrap round)
            if accounted.get(file).is_some_and(|s| s.contains(line_1)) {
                continue;
            }

            let line_0 = line_1 - 1;
            let path = PathBuf::from(file.as_str());

            // Extract identifier tokens from the matched text
            for m in ident_re.find_iter(matched_text) {
                if total_symbols(&result) >= GREP_HOVER_THRESHOLD {
                    break;
                }

                let token = m.as_str();

                // Skip if already a known symbol name or already bootstrapped
                if by_name.contains_key(token) || bootstrapped_names.contains(token) {
                    continue;
                }

                // Column = match start on line + token offset within match text
                let col = match_col + u32::try_from(m.start()).unwrap_or(0);

                // prepareRename: Ok(non-null) → symbol, Ok(null) → keyword.
                // No renameProvider → skip the check, assume symbol.
                let is_symbol = {
                    let open_result = self
                        .client_manager
                        .ensure_document_open(&path, parent_id)
                        .await;
                    if let Ok((uri_str, client_mutex)) = open_result {
                        let mut client = client_mutex.lock().await;
                        client.set_parent_id(parent_id);
                        let result = if client.supports_rename() {
                            let response = client.prepare_rename(&uri_str, line_0, col).await;
                            matches!(response, Ok(ref v) if !v.is_null())
                        } else {
                            true
                        };
                        drop(client);
                        self.client_manager
                            .close_document(&uri_str, &client_mutex)
                            .await;
                        result
                    } else {
                        false
                    }
                };
                if !is_symbol {
                    continue;
                }

                let (kind, resolved, enrichment) = self
                    .enrich_at_position_infer_kind(&path, line_0, col, parent_id)
                    .await;

                // resolved_name comes from prepareCallHierarchy (functions)
                // or prepareTypeHierarchy (types). If neither returned a name,
                // this token isn't a structural symbol — skip it.
                let Some(name) = resolved else { continue };
                // If the resolved name doesn't contain the token, this is a
                // keyword (`fn`, `struct`) that resolved to the adjacent symbol
                // — skip and let the real token get enriched. Substring matches
                // (e.g., token `test_glob` resolving to `test_glob_basic`) are
                // legitimate and should proceed.
                if !name.contains(token) {
                    continue;
                }

                // Skip if this resolved name is already known
                if by_name.contains_key(&name) || bootstrapped_names.contains(&name) {
                    // Still update accounted set so remaining hits are explained
                    for (ref_file, ref_lines) in &enrichment.ref_lines {
                        accounted
                            .entry(ref_file.clone())
                            .or_default()
                            .extend(ref_lines);
                        result
                            .ref_lines
                            .entry(ref_file.clone())
                            .or_default()
                            .extend(ref_lines);
                    }
                    accounted.entry(file.clone()).or_default().insert(*line_1);
                    continue;
                }

                // Update accounted set with new reference lines
                for (ref_file, ref_lines) in &enrichment.ref_lines {
                    accounted
                        .entry(ref_file.clone())
                        .or_default()
                        .extend(ref_lines);
                    result
                        .ref_lines
                        .entry(ref_file.clone())
                        .or_default()
                        .extend(ref_lines);
                }

                // Add definition line to accounted
                accounted.entry(file.clone()).or_default().insert(*line_1);

                // Store enrichment
                result
                    .enrichments
                    .insert((file.clone(), line_0), enrichment);

                let sym = SymbolInfo {
                    name: name.clone(),
                    kind,
                    file_path: file.clone(),
                    line: line_0,
                    character: col,
                };

                if !by_name.contains_key(&name) {
                    result.name_order.push(name.clone());
                }
                result.symbols.push(sym);
                bootstrapped_names.insert(name);
            }
        }

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
        if local.file_lines.is_empty() && local.matched_set.is_empty() {
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
                let text = text.to_string();
                let col = u32::try_from(m.start()).unwrap_or(0);
                self.local.matched_set.insert(text.clone());
                self.local
                    .file_line_texts
                    .entry(self.path.to_string())
                    .or_default()
                    .entry(line_num)
                    .or_default()
                    .push((text, col));
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

// ─── Search enrichment types and formatting ─────────────────────────────

/// Enrichment data collected for a single workspace symbol.
#[derive(Default)]
struct SymbolEnrichment {
    /// Hover content (signature + docs).
    hover: Option<String>,
    /// Reference line numbers per file (for deduplication).
    ref_lines: HashMap<String, HashSet<u32>>,
    /// Incoming calls: `(caller_name, file_path, line_1based)`.
    incoming_calls: Vec<(String, String, u32)>,
    /// Implementation locations: `(file_path, line_1based)`.
    implementations: Vec<(String, u32)>,
    /// Subtypes: `(type_name, file_path, line_1based)`.
    subtypes: Vec<(String, String, u32)>,
}

/// Symbols and enrichment discovered by rg-bootstrapped hover.
struct BootstrapResult {
    /// Newly-discovered `SymbolInfo` values.
    symbols: Vec<SymbolInfo>,
    /// Enrichment keyed by `(file_path, line_0based)`.
    enrichments: HashMap<(String, u32), SymbolEnrichment>,
    /// Reference lines discovered during bootstrap, keyed by file path.
    ref_lines: HashMap<String, HashSet<u32>>,
    /// Names of bootstrapped symbols in discovery order.
    name_order: Vec<String>,
}

/// Result of a ripgrep `--only-matching` search.
#[derive(Default)]
struct RipgrepMatches {
    /// Unique matched strings (for LSP queries).
    matched_strings: Vec<String>,
    /// Per-file line numbers (for heatmap tier).
    file_lines: BTreeMap<String, Vec<u32>>,
    /// Per-file, per-line matched texts with column offsets
    /// `(matched_text, column_byte_offset)` for routing unclaimed lines
    /// to headings and for rg-bootstrapped hover positions.
    file_line_texts: HashMap<String, HashMap<u32, Vec<(String, u32)>>>,
}

impl RipgrepMatches {
    /// Merges per-thread match accumulators into a single result.
    fn merge(parts: Vec<ThreadMatches>) -> Self {
        let mut file_lines: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        let mut matched_set: HashSet<String> = HashSet::new();
        let mut file_line_texts: HashMap<String, HashMap<u32, Vec<(String, u32)>>> = HashMap::new();

        for part in parts {
            for (file, lines) in part.file_lines {
                file_lines.entry(file).or_default().extend(lines);
            }
            matched_set.extend(part.matched_set);
            for (file, line_map) in part.file_line_texts {
                let entry = file_line_texts.entry(file).or_default();
                for (line, texts) in line_map {
                    entry.entry(line).or_default().extend(texts);
                }
            }
        }

        Self {
            matched_strings: matched_set.into_iter().collect(),
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
    /// Unique matched strings (`HashSet` for efficient per-thread dedup).
    matched_set: HashSet<String>,
    /// Per-file, per-line matched texts with column offsets.
    file_line_texts: HashMap<String, HashMap<u32, Vec<(String, u32)>>>,
}

/// Extracts plain text from a `Value`-based LSP hover response.
fn extract_hover_text_from_value(hover: &Value) -> Option<String> {
    let contents = hover.get("contents")?;

    // String: plain MarkedString
    if let Some(s) = contents.as_str() {
        let s = s.trim();
        return if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        };
    }

    // Object with "value" field: MarkupContent or LanguageString MarkedString
    if let Some(value) = contents.get("value").and_then(Value::as_str) {
        let text = value.trim();
        return if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        };
    }

    // Array of MarkedString
    if let Some(arr) = contents.as_array() {
        let texts: Vec<String> = arr
            .iter()
            .filter_map(|item| {
                item.as_str().map_or_else(
                    || {
                        item.get("value")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    },
                    |s| Some(s.to_string()),
                )
            })
            .collect();
        return if texts.is_empty() {
            None
        } else {
            Some(texts.join("\n"))
        };
    }

    None
}

/// Extracts the file path from a hierarchy item's `uri` field.
fn value_file_path(item: &Value) -> String {
    item.get("uri")
        .and_then(Value::as_str)
        .and_then(symbols::uri_to_path)
        .unwrap_or_default()
}

/// Extracts `range.start.line` from a hierarchy item `Value`.
fn value_start_line(item: &Value) -> u32 {
    item.get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0)
}

/// Extracts incoming calls from a `callHierarchy/incomingCalls` response
/// into the enrichment's `incoming_calls` list.
fn extract_incoming_calls(response: &Value, enrichment: &mut SymbolEnrichment) {
    if let Some(calls) = response.as_array() {
        for call in calls {
            if let Some(from) = call.get("from") {
                enrichment.incoming_calls.push((
                    from.get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    value_file_path(from),
                    value_start_line(from) + 1,
                ));
            }
        }
    }
}

/// Extracts subtypes from a `typeHierarchy/subtypes` response
/// into the enrichment's `subtypes` list.
fn extract_subtypes(response: &Value, enrichment: &mut SymbolEnrichment) {
    if let Some(subs) = response.as_array() {
        for sub in subs {
            enrichment.subtypes.push((
                sub.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                value_file_path(sub),
                value_start_line(sub) + 1,
            ));
        }
    }
}

/// Formats sorted line numbers as compact ranges: `L45`, `L15-L20`.
/// Nearby lines are clustered using sqrt-based merge distance (DBSCAN-style):
/// `merge_distance = ceil(sqrt(max_line))`. This scales with file size —
/// small files cluster tightly, large files tolerate wider gaps.
fn format_line_ranges(lines: &[u32]) -> String {
    if lines.is_empty() {
        return String::new();
    }

    let mut sorted = lines.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let max_line = sorted[sorted.len() - 1];
    // Integer ceiling of sqrt: isqrt rounds down, so add 1 unless it's a perfect square.
    let isqrt = u32::isqrt(max_line);
    let merge_distance = if isqrt * isqrt == max_line {
        isqrt
    } else {
        isqrt + 1
    }
    .max(1);

    let mut ranges: Vec<String> = Vec::new();
    let mut start = sorted[0];
    let mut end = sorted[0];

    for &line in &sorted[1..] {
        if line - end > merge_distance {
            ranges.push(format_single_range(start, end));
            start = line;
        }
        end = line;
    }
    ranges.push(format_single_range(start, end));

    ranges.join(" ")
}

/// Formats a single line or range.
fn format_single_range(start: u32, end: u32) -> String {
    if start == end {
        format!("L{start}")
    } else {
        format!("L{start}-L{end}")
    }
}

/// Formats references from enrichment, excluding the definition line
/// and any lines already shown in labeled sections.
fn format_symbol_references(
    enrichment: &SymbolEnrichment,
    def_file: &str,
    def_line_0: u32,
    fs: &FilesystemManager,
    labeled_lines: &HashSet<(String, u32)>,
) -> String {
    use std::fmt::Write;

    let def_line_1 = def_line_0 + 1;
    let mut output = String::new();

    // Sort files for stable output
    let mut files: Vec<&String> = enrichment.ref_lines.keys().collect();
    files.sort();

    for file in files {
        let lines = &enrichment.ref_lines[file];
        // Filter out the definition line and labeled lines
        let is_def_file = file.as_str() == def_file;
        let mut filtered: Vec<u32> = lines
            .iter()
            .copied()
            .filter(|&l| {
                if is_def_file && l == def_line_1 {
                    return false;
                }
                !labeled_lines.contains(&(file.clone(), l))
            })
            .collect();
        if filtered.is_empty() {
            continue;
        }
        filtered.sort_unstable();
        let path = display_path(file, fs);
        let _ = writeln!(output, "{path} {}", format_line_ranges(&filtered));
    }

    let trimmed_len = output.trim_end().len();
    output.truncate(trimmed_len);
    output
}

/// Assigns rg file/line hits to symbol names based on LSP reference data
/// and matched text routing.
///
/// Returns a map from heading name to file hits. Each heading name is either
/// an LSP symbol name or an rg-only matched string.
fn assign_rg_lines_to_symbols(
    by_name: &BTreeMap<String, Vec<&SymbolInfo>>,
    all_ref_lines: &HashMap<String, HashSet<u32>>,
    rg: &RipgrepMatches,
) -> BTreeMap<String, Vec<(String, Vec<u32>)>> {
    let mut result: BTreeMap<String, Vec<(String, Vec<u32>)>> = BTreeMap::new();

    // Track which rg lines are claimed by any symbol's references
    let mut claimed: HashMap<String, HashSet<u32>> = HashMap::new();
    for (file, ref_set) in all_ref_lines {
        claimed.entry(file.clone()).or_default().extend(ref_set);
    }

    // Also claim definition lines
    for defs in by_name.values() {
        for sym in defs {
            claimed
                .entry(sym.file_path.clone())
                .or_default()
                .insert(sym.line + 1);
        }
    }

    // Build a lowercase lookup for symbol names
    let name_lower: Vec<(String, String)> = by_name
        .keys()
        .map(|n: &String| (n.clone(), n.to_lowercase()))
        .collect();

    // For each rg file, find unclaimed lines and route by matched text
    for (file, lines) in &rg.file_lines {
        let unclaimed: Vec<u32> = lines
            .iter()
            .copied()
            .filter(|l| !claimed.get(file.as_str()).is_some_and(|s| s.contains(l)))
            .collect();
        if unclaimed.is_empty() {
            continue;
        }

        // Group unclaimed lines by their matched text, then route to headings
        let file_texts = rg.file_line_texts.get(file.as_str());
        // Collect (heading, lines) per heading for this file
        let mut heading_lines: BTreeMap<String, Vec<u32>> = BTreeMap::new();

        for &line_num in &unclaimed {
            let texts = file_texts.and_then(|ft| ft.get(&line_num));
            let mut routed = false;

            if let Some(texts) = texts {
                for (matched_text, _col) in texts {
                    let mt_lower = matched_text.to_lowercase();
                    // Try to match to a known symbol name
                    for (name, nl) in &name_lower {
                        if mt_lower == *nl || mt_lower.contains(nl.as_str()) {
                            heading_lines
                                .entry(name.clone())
                                .or_default()
                                .push(line_num);
                            routed = true;
                            break;
                        }
                    }
                    if !routed {
                        // No symbol match — use the matched text itself as heading
                        heading_lines
                            .entry(matched_text.clone())
                            .or_default()
                            .push(line_num);
                        routed = true;
                    }
                    if routed {
                        break;
                    }
                }
            }

            if !routed {
                // No matched text info — fallback to empty key
                heading_lines
                    .entry(String::new())
                    .or_default()
                    .push(line_num);
            }
        }

        for (heading, lines) in heading_lines {
            result
                .entry(heading)
                .or_default()
                .push((file.clone(), lines));
        }
    }

    result
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

    // ─── format_line_ranges tests ────────────────────────────────────────

    #[test]
    fn test_format_line_ranges_empty() {
        assert_eq!(format_line_ranges(&[]), "");
    }

    #[test]
    fn test_format_line_ranges_single() {
        assert_eq!(format_line_ranges(&[42]), "L42");
    }

    #[test]
    fn test_format_line_ranges_consecutive() {
        assert_eq!(format_line_ranges(&[10, 11, 12]), "L10-L12");
    }

    #[test]
    fn test_format_line_ranges_disjoint() {
        // max=20, merge_distance=ceil(sqrt(20))=5
        // gap 5→10 = 5 ≤ 5 → merge; gap 10→20 = 10 > 5 → split
        assert_eq!(format_line_ranges(&[5, 10, 20]), "L5-L10 L20");
    }

    #[test]
    fn test_format_line_ranges_mixed() {
        // max=50, merge_distance=ceil(sqrt(50))=8
        // gaps: 3→10=7 ≤ 8, 11→50=39 > 8
        assert_eq!(format_line_ranges(&[1, 2, 3, 10, 11, 50]), "L1-L11 L50");
    }

    #[test]
    fn test_format_line_ranges_unsorted() {
        // max=20, merge_distance=5; sorted [10,11,20]; gap 11→20=9 > 5
        assert_eq!(format_line_ranges(&[20, 10, 11]), "L10-L11 L20");
    }

    #[test]
    fn test_format_line_ranges_dbscan_nearby() {
        // max=30, merge_distance=ceil(sqrt(30))=6; gap 25→30=5 ≤ 6
        assert_eq!(format_line_ranges(&[25, 30]), "L25-L30");
    }

    #[test]
    fn test_format_line_ranges_dbscan_far_apart() {
        // max=1000, merge_distance=ceil(sqrt(1000))=32; gap=999 > 32
        assert_eq!(format_line_ranges(&[1, 1000]), "L1 L1000");
    }

    #[test]
    fn test_format_line_ranges_dbscan_mixed() {
        // max=14, merge_distance=ceil(sqrt(14))=4
        // gap 5→10=5 > 4 → split; gap 10→14=4 ≤ 4 → merge
        assert_eq!(format_line_ranges(&[5, 10, 14]), "L5 L10-L14");
    }

    // ─── format_symbol_references tests ──────────────────────────────────

    #[test]
    fn test_format_symbol_references_excludes_def() {
        let mut ref_lines = HashMap::new();
        ref_lines.insert("/src/lib.rs".to_string(), HashSet::from([1, 10, 20]));

        let enrichment = SymbolEnrichment {
            hover: None,
            ref_lines,
            ..SymbolEnrichment::default()
        };
        let fs = FilesystemManager::new();
        fs.set_roots(vec![PathBuf::from("/")]);
        let labeled = HashSet::new();

        // Definition is at line 0 (0-indexed) = line 1 (1-indexed)
        let result = format_symbol_references(&enrichment, "/src/lib.rs", 0, &fs, &labeled);
        assert!(result.contains("L10"));
        assert!(result.contains("L20"));
        assert!(
            !result.contains("L1 "),
            "Definition line should be excluded"
        );
    }
}

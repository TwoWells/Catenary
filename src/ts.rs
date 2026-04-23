// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Tree-sitter index for workspace-wide symbol extraction.
//!
//! Provides [`TsIndex`], a SQLite-backed symbol index built from tree-sitter
//! grammars. The index walks workspace files, parses them using installed
//! grammars, and writes extracted symbols to the database. `build()` creates
//! the initial index, `query()` reads it with regex filtering, and
//! `update_file()` incrementally re-parses a single file. Callers are
//! responsible for ensuring freshness via `update_file()` before querying.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use rusqlite::Connection;
use streaming_iterator::StreamingIterator;
use tracing::info;

/// A symbol extracted from the tree-sitter index.
pub struct TsSymbol {
    /// Symbol name.
    pub name: String,
    /// Capture suffix from tags.scm (e.g., `"function"`, `"implementation"`).
    pub kind: String,
    /// 0-based start line of the definition.
    pub line: u32,
    /// 0-based end line of the definition (for structure spans).
    pub end_line: u32,
    /// Container name (enclosing definition's name).
    pub scope: Option<String>,
    /// Container kind (enclosing definition's capture suffix).
    pub scope_kind: Option<String>,
}

/// Workspace-wide tree-sitter index backed by in-memory `SQLite`.
///
/// Grammars are loaded from the filesystem (grammar directory with
/// `metadata.json` sidecars). The symbol index is ephemeral — built on
/// session start, rebuilt on file changes, discarded on session end.
/// No dependency on the persistent session database.
pub struct TsIndex {
    /// In-memory connection for symbol reads and writes.
    conn: Connection,
    /// Loaded grammars keyed by scope (e.g. `"source.rust"`).
    grammars: HashMap<String, tree_sitter::Language>,
    /// Scope → file extensions, from grammar metadata.
    extensions: HashMap<String, Vec<String>>,
    /// tags.scm queries keyed by scope.
    tag_queries: HashMap<String, tree_sitter::Query>,
    /// File extension → grammar scope lookup for `update_file()`.
    ext_to_scope: HashMap<String, String>,
}

/// Display labels for kind brackets in output.
///
/// NOT a query gate — all enrichment queries are sent for every
/// tier 1 symbol regardless of category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrichmentCategory {
    /// Functions, methods, constructors, macros, etc.
    Callable,
    /// Structs, classes, enums, traits, interfaces, etc.
    Type,
    /// Everything else.
    Other,
}

static CALLABLE_KINDS: LazyLock<HashSet<&str>> = LazyLock::new(|| {
    HashSet::from([
        "function",
        "method",
        "constructor",
        "macro",
        "subroutine",
        "command",
        "procedure",
    ])
});

static TYPE_KINDS: LazyLock<HashSet<&str>> = LazyLock::new(|| {
    HashSet::from([
        "struct",
        "class",
        "enum",
        "trait",
        "interface",
        "union",
        "typedef",
        "type",
        "protocol",
    ])
});

/// Categorize a kind suffix into an [`EnrichmentCategory`].
///
/// Uses `HashSet` lookups against the callable and type kind tables.
#[must_use]
pub fn categorize(kind: &str) -> EnrichmentCategory {
    if CALLABLE_KINDS.contains(kind) {
        EnrichmentCategory::Callable
    } else if TYPE_KINDS.contains(kind) {
        EnrichmentCategory::Type
    } else {
        EnrichmentCategory::Other
    }
}

/// Title-case a capture suffix for display brackets.
///
/// Special case: `"implementation"` → `"Impl"`. All others: first char
/// uppercase, rest lowercase.
#[must_use]
pub fn format_ts_kind(capture_suffix: &str) -> String {
    if capture_suffix == "implementation" {
        return "Impl".to_string();
    }
    let mut chars = capture_suffix.chars();
    chars.next().map_or_else(String::new, |first| {
        let mut s = first.to_uppercase().to_string();
        for ch in chars {
            s.extend(ch.to_lowercase());
        }
        s
    })
}

/// LSP abbreviation table for edge labels (calls, supertypes, subtypes).
///
/// Maps LSP `SymbolKind` numeric values to short display labels.
/// Unknown kinds return `"Sym"`.
#[must_use]
pub const fn lsp_kind_label(kind: u32) -> &'static str {
    match kind {
        1 => "File",
        2 => "Mod",
        3 => "Ns",
        4 => "Pkg",
        5 => "Class",
        6 => "Method",
        7 => "Prop",
        8 => "Field",
        9 => "Ctor",
        10 => "Enum",
        11 => "Iface",
        12 => "Fn",
        13 => "Var",
        14 => "Const",
        15 => "Str",
        16 => "Num",
        17 => "Bool",
        18 => "Array",
        19 => "Obj",
        20 => "Key",
        21 => "Null",
        22 => "Member",
        23 => "Struct",
        24 => "Event",
        25 => "Op",
        26 => "TypeParam",
        _ => "Sym",
    }
}

/// Intermediate representation for a definition found during parsing.
struct RawDef {
    name: String,
    kind: String,
    start_byte: usize,
    end_byte: usize,
    start_line: u32,
    end_line: u32,
}

/// Read a file's mtime as nanoseconds since the Unix epoch.
///
/// Returns 0 if the metadata or system time conversion fails.
fn file_mtime_ns(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0i64, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// Parse a single file with a tree-sitter grammar and extract symbols.
///
/// Runs the query cursor over the parse tree, collects `RawDef` entries,
/// then resolves scopes using a stack of enclosing definitions.
fn parse_file(
    source: &str,
    language: &tree_sitter::Language,
    query: &tree_sitter::Query,
) -> Result<Vec<TsSymbol>> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(language)
        .map_err(|e| anyhow::anyhow!("failed to set parser language: {e}"))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter parse returned None"))?;

    let source_bytes = source.as_bytes();
    let capture_names = query.capture_names();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut query_matches = cursor.matches(query, tree.root_node(), source_bytes);

    let mut defs: Vec<RawDef> = Vec::new();

    while let Some(m) = query_matches.next() {
        let mut name_text: Option<String> = None;
        let mut def_info: Option<(String, usize, usize, u32, u32)> = None;

        for capture in m.captures {
            let cap_name: &str = capture_names[capture.index as usize];
            if cap_name == "name" {
                if let Ok(text) = capture.node.utf8_text(source_bytes) {
                    name_text = Some(text.to_string());
                }
            } else if let Some(suffix) = cap_name.strip_prefix("definition.") {
                let node = capture.node;
                def_info = Some((
                    suffix.to_string(),
                    node.start_byte(),
                    node.end_byte(),
                    u32::try_from(node.start_position().row).unwrap_or(u32::MAX),
                    u32::try_from(node.end_position().row).unwrap_or(u32::MAX),
                ));
            }
        }

        if let (Some(name), Some((kind, sb, eb, sl, el))) = (name_text, def_info) {
            defs.push(RawDef {
                name,
                kind,
                start_byte: sb,
                end_byte: eb,
                start_line: sl,
                end_line: el,
            });
        }
    }

    // Sort by start_byte for scope determination.
    defs.sort_by_key(|d| d.start_byte);

    // Determine scopes using a stack of (name, kind, end_byte).
    let mut scope_stack: Vec<(&str, &str, usize)> = Vec::new();
    let mut symbols: Vec<TsSymbol> = Vec::new();

    for def in &defs {
        while scope_stack
            .last()
            .is_some_and(|&(_, _, eb)| eb <= def.start_byte)
        {
            scope_stack.pop();
        }

        let (scope_name, scope_kind) = scope_stack.last().map_or((None, None), |&(n, k, _)| {
            (Some(n.to_string()), Some(k.to_string()))
        });

        symbols.push(TsSymbol {
            name: def.name.clone(),
            kind: def.kind.clone(),
            line: def.start_line,
            end_line: def.end_line,
            scope: scope_name,
            scope_kind,
        });

        scope_stack.push((&def.name, &def.kind, def.end_byte));
    }

    Ok(symbols)
}

impl TsIndex {
    /// Build the tree-sitter index from workspace roots.
    ///
    /// Scans the grammar directory for installed grammars (`metadata.json`
    /// sidecars), walks workspace roots to find files matching grammar file
    /// types, parses each file, and writes symbols to an in-memory `SQLite`
    /// database. No persistent database dependency.
    ///
    /// # Errors
    ///
    /// Returns an error if grammar loading fails or the in-memory database
    /// operations fail.
    #[allow(clippy::too_many_lines, reason = "sequential pipeline steps")]
    pub fn build(roots: &[PathBuf]) -> Result<Self> {
        Self::build_with_grammar_dir(roots, &crate::install::grammar_dir())
    }

    /// Build the tree-sitter index with an explicit grammar directory.
    ///
    /// Like [`build`](Self::build) but uses the given directory instead of
    /// the default grammar location. Used by tests.
    ///
    /// # Errors
    ///
    /// Returns an error if grammar loading fails or the in-memory database
    /// operations fail.
    #[allow(clippy::too_many_lines, reason = "sequential pipeline steps")]
    pub fn build_with_grammar_dir(roots: &[PathBuf], grammar_base: &Path) -> Result<Self> {
        let mut grammars = HashMap::new();
        let mut extensions = HashMap::new();
        let mut tag_queries = HashMap::new();
        let mut ext_to_scope: HashMap<String, String> = HashMap::new();

        // Step 1: Load installed grammars from the filesystem.
        let grammar_metas = crate::install::scan_grammars_in(grammar_base).unwrap_or_default();

        for meta in &grammar_metas {
            let scope = &meta.scope;
            let scope_dir = grammar_base.join(scope);

            let lib_filename = format!("parser.{}", std::env::consts::DLL_EXTENSION);
            let lib_path = scope_dir.join(&lib_filename);
            let tags_path = scope_dir.join("tags.scm");

            if !lib_path.exists() || !tags_path.exists() {
                info!(scope, "skipping grammar — missing files");
                continue;
            }

            let lang_name = scope
                .rsplit('.')
                .next()
                .ok_or_else(|| anyhow::anyhow!("invalid scope: {scope}"))?;
            let symbol_name = format!("tree_sitter_{lang_name}");

            let language = catenary_ts::load_grammar(&lib_path, &symbol_name)
                .with_context(|| format!("failed to load grammar for {scope}"))?;

            let tags_source = std::fs::read_to_string(&tags_path)
                .with_context(|| format!("failed to read tags.scm for {scope}"))?;
            let query = tree_sitter::Query::new(&language, &tags_source)
                .map_err(|e| anyhow::anyhow!("failed to compile tags.scm for {scope}: {e}"))?;

            for ext in &meta.file_types {
                ext_to_scope.insert(ext.clone(), scope.clone());
            }

            grammars.insert(scope.clone(), language);
            extensions.insert(scope.clone(), meta.file_types.clone());
            tag_queries.insert(scope.clone(), query);
        }

        // Step 2: Create in-memory SQLite for the symbol index.
        let conn = Connection::open_in_memory().context("failed to open in-memory database")?;
        conn.execute_batch(
            "CREATE TABLE symbols (
                file_path   TEXT NOT NULL,
                name        TEXT NOT NULL,
                kind        TEXT NOT NULL,
                line        INTEGER NOT NULL,
                end_line    INTEGER NOT NULL,
                scope       TEXT,
                scope_kind  TEXT,
                PRIMARY KEY (file_path, line)
            );
            CREATE INDEX idx_symbols_name ON symbols(name);
            CREATE TABLE file_parse_state (
                file_path   TEXT PRIMARY KEY,
                mtime_ns    INTEGER NOT NULL,
                grammar     TEXT NOT NULL
            );",
        )
        .context("failed to create in-memory tables")?;

        conn.create_scalar_function(
            "regexp",
            2,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8
                | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let pattern = ctx.get_raw(0).as_str()?;
                let text = ctx.get_raw(1).as_str()?;
                let re = regex::Regex::new(pattern)
                    .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
                Ok(re.is_match(text))
            },
        )
        .context("failed to register REGEXP function")?;

        // Step 3: Walk workspace roots and collect symbols.
        let mut all_symbols: Vec<(String, Vec<TsSymbol>)> = Vec::new();
        let mut file_states: Vec<(String, i64, String)> = Vec::new();

        if !roots.is_empty() && !ext_to_scope.is_empty() {
            let mut builder = ignore::WalkBuilder::new(&roots[0]);
            for root in &roots[1..] {
                builder.add(root);
            }
            builder.hidden(false);

            for entry in builder.build() {
                let Ok(entry) = entry else { continue };

                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    continue;
                }

                let path = entry.path();

                let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                    continue;
                };
                let Some(scope) = ext_to_scope.get(ext) else {
                    continue;
                };
                let Some(language) = grammars.get(scope) else {
                    continue;
                };
                let Some(query) = tag_queries.get(scope) else {
                    continue;
                };

                let Ok(source) = std::fs::read_to_string(path) else {
                    continue;
                };

                let mtime = file_mtime_ns(path);

                let Ok(symbols) = parse_file(&source, language, query) else {
                    continue;
                };

                let path_str = path.to_string_lossy().to_string();
                file_states.push((path_str.clone(), mtime, scope.clone()));
                all_symbols.push((path_str, symbols));
            }
        }

        // Step 4: Write symbols to in-memory SQLite.
        {
            let tx = conn.unchecked_transaction().context("begin transaction")?;

            for (file_path, symbols) in &all_symbols {
                for sym in symbols {
                    tx.execute(
                        "INSERT INTO symbols \
                         (file_path, name, kind, line, end_line, scope, scope_kind) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        rusqlite::params![
                            file_path,
                            sym.name,
                            sym.kind,
                            sym.line,
                            sym.end_line,
                            sym.scope,
                            sym.scope_kind,
                        ],
                    )
                    .with_context(|| {
                        format!("failed to insert symbol {} in {}", sym.name, file_path)
                    })?;
                }
            }

            for (file_path, mtime_ns, grammar_scope) in &file_states {
                tx.execute(
                    "INSERT INTO file_parse_state (file_path, mtime_ns, grammar) \
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![file_path, mtime_ns, grammar_scope],
                )
                .context("failed to insert file_parse_state")?;
            }

            tx.commit().context("commit transaction")?;
        }

        let symbol_count: usize = all_symbols.iter().map(|(_, syms)| syms.len()).sum();
        info!(
            files = all_symbols.len(),
            symbols = symbol_count,
            "tree-sitter index built"
        );

        Ok(Self {
            conn,
            grammars,
            extensions,
            tag_queries,
            ext_to_scope,
        })
    }

    /// Incrementally re-parse a single file and update its symbols.
    ///
    /// Looks up the file's grammar scope by extension. If no grammar matches,
    /// returns `Ok(())`. Otherwise deletes the file's existing symbols, parses
    /// the file, inserts fresh symbols, and updates the mtime record.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, parsing fails, or the
    /// database transaction fails.
    pub fn update_file(&self, path: &Path) -> Result<()> {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return Ok(());
        };
        let Some(scope) = self.ext_to_scope.get(ext) else {
            return Ok(());
        };
        let Some(language) = self.grammars.get(scope) else {
            return Ok(());
        };
        let Some(query) = self.tag_queries.get(scope) else {
            return Ok(());
        };

        let source = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let symbols = parse_file(&source, language, query)?;
        let mtime = file_mtime_ns(path);
        let path_str = path.to_string_lossy();

        let tx = self
            .conn
            .unchecked_transaction()
            .context("begin transaction")?;

        tx.execute(
            "DELETE FROM symbols WHERE file_path = ?1",
            rusqlite::params![path_str.as_ref() as &str],
        )
        .context("failed to delete old symbols")?;

        for sym in &symbols {
            tx.execute(
                "INSERT INTO symbols \
                 (file_path, name, kind, line, end_line, scope, scope_kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    path_str.as_ref() as &str,
                    sym.name,
                    sym.kind,
                    sym.line,
                    sym.end_line,
                    sym.scope,
                    sym.scope_kind,
                ],
            )
            .with_context(|| format!("failed to insert symbol {} in {}", sym.name, path_str))?;
        }

        tx.execute(
            "INSERT OR REPLACE INTO file_parse_state (file_path, mtime_ns, grammar) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![path_str.as_ref() as &str, mtime, scope],
        )
        .context("failed to update file_parse_state")?;

        tx.commit().context("commit transaction")?;
        Ok(())
    }

    /// Query the index for symbols whose names match a regex pattern.
    ///
    /// If `files` is `Some`, only symbols from those files are returned.
    ///
    /// # Errors
    ///
    /// Returns an error if the regex is invalid or the query fails.
    pub fn query(
        &self,
        pattern: &str,
        files: Option<&[PathBuf]>,
    ) -> Result<Vec<(PathBuf, TsSymbol)>> {
        let mut results = Vec::new();

        match files {
            Some(file_list) if !file_list.is_empty() => {
                let placeholders: String = (0..file_list.len())
                    .map(|i| format!("?{}", i + 2))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "SELECT file_path, name, kind, line, end_line, scope, scope_kind \
                     FROM symbols WHERE name REGEXP ?1 AND file_path IN ({placeholders})"
                );
                let mut stmt = self.conn.prepare(&sql).context("failed to prepare query")?;

                let mut params: Vec<Box<dyn rusqlite::types::ToSql>> =
                    Vec::with_capacity(1 + file_list.len());
                params.push(Box::new(pattern.to_string()));
                for f in file_list {
                    params.push(Box::new(f.to_string_lossy().to_string()));
                }
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params.iter().map(AsRef::as_ref).collect();

                let rows = stmt
                    .query_map(param_refs.as_slice(), Self::row_to_symbol)
                    .context("failed to execute query")?;
                for row in rows {
                    results.push(row.context("failed to read symbol row")?);
                }
            }
            _ => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT file_path, name, kind, line, end_line, scope, scope_kind \
                         FROM symbols WHERE name REGEXP ?1",
                    )
                    .context("failed to prepare query")?;
                let rows = stmt
                    .query_map([pattern], Self::row_to_symbol)
                    .context("failed to execute query")?;
                for row in rows {
                    results.push(row.context("failed to read symbol row")?);
                }
            }
        }

        Ok(results
            .into_iter()
            .map(|(p, sym)| (PathBuf::from(p), sym))
            .collect())
    }

    /// Query depth-0 (outline) symbols for a batch of files.
    ///
    /// Returns symbols with `scope IS NULL` grouped by file path,
    /// ordered by line number within each file. Used by the glob tool
    /// for defensive maps.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub fn query_outline_batch(&self, files: &[&Path]) -> Result<HashMap<PathBuf, Vec<TsSymbol>>> {
        if files.is_empty() {
            return Ok(HashMap::new());
        }

        let placeholders: String = (0..files.len())
            .map(|i| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "SELECT file_path, name, kind, line, end_line, scope, scope_kind \
             FROM symbols \
             WHERE file_path IN ({placeholders}) AND scope IS NULL \
             ORDER BY file_path, line"
        );

        let mut stmt = self.conn.prepare(&sql).context("prepare outline batch")?;
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::with_capacity(files.len());
        for f in files {
            params.push(Box::new(f.to_string_lossy().to_string()));
        }
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(AsRef::as_ref).collect();

        let rows = stmt
            .query_map(param_refs.as_slice(), Self::row_to_symbol)
            .context("execute outline batch")?;

        let mut result: HashMap<PathBuf, Vec<TsSymbol>> = HashMap::new();
        for row in rows {
            let (path_str, sym) = row.context("read outline row")?;
            result.entry(PathBuf::from(path_str)).or_default().push(sym);
        }

        Ok(result)
    }

    /// Finds the innermost symbol enclosing a line in a file.
    ///
    /// Returns the tightest definition (smallest span) containing the given
    /// 0-based line.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub fn find_enclosing(&self, file_path: &Path, line_0: u32) -> Result<Option<TsSymbol>> {
        let path_str = file_path.to_string_lossy();
        let mut stmt = self.conn.prepare(
            "SELECT name, kind, line, end_line, scope, scope_kind \
             FROM symbols \
             WHERE file_path = ?1 AND line <= ?2 AND end_line >= ?2 \
             ORDER BY (end_line - line) ASC \
             LIMIT 1",
        )?;

        let result = stmt
            .query_row(
                rusqlite::params![path_str.as_ref() as &str, line_0],
                |row| {
                    Ok(TsSymbol {
                        name: row.get(0)?,
                        kind: row.get(1)?,
                        line: row.get(2)?,
                        end_line: row.get(3)?,
                        scope: row.get(4)?,
                        scope_kind: row.get(5)?,
                    })
                },
            )
            .ok();

        Ok(result)
    }

    /// Map a database row to a `(file_path, TsSymbol)` pair.
    fn row_to_symbol(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, TsSymbol)> {
        Ok((
            row.get(0)?,
            TsSymbol {
                name: row.get(1)?,
                kind: row.get(2)?,
                line: row.get(3)?,
                end_line: row.get(4)?,
                scope: row.get(5)?,
                scope_kind: row.get(6)?,
            },
        ))
    }

    /// Check whether a grammar is installed for the file's language.
    ///
    /// Matches the file's extension against all installed grammar file types.
    #[must_use]
    pub fn has_grammar_for(&self, path: &Path) -> bool {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return false;
        };
        self.extensions
            .values()
            .any(|exts| exts.iter().any(|e| e == ext))
    }

    /// Ensures the index is fresh for the given files.
    ///
    /// For each file, checks `file_parse_state.mtime_ns` against the current
    /// filesystem mtime. Files that are missing from `file_parse_state` or whose
    /// current mtime exceeds the stored value are re-parsed via `update_file()`.
    ///
    /// # Errors
    ///
    /// Returns an error if a database query or file re-parse fails.
    pub fn ensure_fresh(&self, files: &[PathBuf]) -> Result<()> {
        for path in files {
            let path_str = path.to_string_lossy();
            let stored_mtime: Option<i64> = self
                .conn
                .query_row(
                    "SELECT mtime_ns FROM file_parse_state WHERE file_path = ?1",
                    rusqlite::params![path_str.as_ref() as &str],
                    |row| row.get(0),
                )
                .ok();

            let current_mtime = file_mtime_ns(path);

            let needs_update = stored_mtime.map_or_else(
                || self.has_grammar_for(path),
                |stored| current_mtime > stored,
            );

            if needs_update {
                self.update_file(path)?;
            }
        }
        Ok(())
    }

    /// Check whether a scope (container) has children in the given file.
    ///
    /// Returns `true` if any symbol in the index has `scope = scope_name`
    /// within the given file path.
    #[must_use]
    pub fn has_children(&self, file_path: &Path, scope_name: &str) -> bool {
        let path_str = file_path.to_string_lossy();
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM symbols WHERE file_path = ?1 AND scope = ?2)",
                rusqlite::params![path_str.as_ref() as &str, scope_name],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{EnrichmentCategory, TsIndex, categorize, format_ts_kind, lsp_kind_label};

    fn fixture_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test_assets")
            .join("mock_grammar")
    }

    /// Installs the mock grammar fixture into a temporary directory.
    ///
    /// Returns `(tempdir, grammar_base_path)`. The tempdir must be kept
    /// alive for the duration of the test.
    #[allow(clippy::expect_used, reason = "test setup")]
    fn install_mock_grammar() -> (tempfile::TempDir, std::path::PathBuf) {
        let data_dir = tempfile::tempdir().expect("data tempdir");
        let grammar_base = data_dir.path().join("grammars");
        std::fs::create_dir_all(&grammar_base).expect("create grammar dir");
        crate::install::install_from_dir(
            &fixture_dir(),
            &grammar_base,
            "https://github.com/test/mock",
        )
        .expect("install mock grammar");
        (data_dir, grammar_base)
    }

    // --- Pure unit tests (no grammar needed) ---

    #[test]
    fn test_format_ts_kind() {
        assert_eq!(format_ts_kind("function"), "Function");
        assert_eq!(format_ts_kind("implementation"), "Impl");
        assert_eq!(format_ts_kind("struct"), "Struct");
        assert_eq!(format_ts_kind("method"), "Method");
    }

    #[test]
    fn test_categorize() {
        assert_eq!(categorize("function"), EnrichmentCategory::Callable);
        assert_eq!(categorize("struct"), EnrichmentCategory::Type);
        assert_eq!(categorize("variable"), EnrichmentCategory::Other);
        assert_eq!(categorize("unknown"), EnrichmentCategory::Other);
    }

    #[test]
    fn test_lsp_kind_label() {
        assert_eq!(lsp_kind_label(12), "Fn");
        assert_eq!(lsp_kind_label(11), "Iface");
        assert_eq!(lsp_kind_label(2), "Mod");
        assert_eq!(lsp_kind_label(999), "Sym");
    }

    // --- Tests using the extended mock grammar ---

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_build_index_parses_symbols() {
        let (_data_dir, grammar_base) = install_mock_grammar();

        let workspace = tempfile::tempdir().expect("workspace tempdir");
        std::fs::write(
            workspace.path().join("test.mock"),
            "fn foo\nstruct Bar\nfn baz",
        )
        .expect("write test file");

        let index =
            TsIndex::build_with_grammar_dir(&[workspace.path().to_path_buf()], &grammar_base)
                .expect("build index");

        // Verify symbols via query
        let results = index.query(".*", None).expect("query all");
        assert_eq!(results.len(), 3, "expected 3 symbols");

        let names: Vec<&str> = results.iter().map(|(_, s)| s.name.as_str()).collect();
        assert!(names.contains(&"foo"));
        assert!(names.contains(&"Bar"));
        assert!(names.contains(&"baz"));

        let foo = results.iter().find(|(_, s)| s.name == "foo").expect("foo");
        assert_eq!(foo.1.kind, "function");
        assert_eq!(foo.1.line, 0);
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_no_grammar_skip() {
        let (_data_dir, grammar_base) = install_mock_grammar();

        let workspace = tempfile::tempdir().expect("workspace tempdir");
        std::fs::write(workspace.path().join("readme.txt"), "hello world").expect("write txt file");

        let index =
            TsIndex::build_with_grammar_dir(&[workspace.path().to_path_buf()], &grammar_base)
                .expect("build index");

        let results = index.query(".*", None).expect("query all");
        assert!(results.is_empty(), "no symbols for .txt files");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_has_grammar_for() {
        let (_data_dir, grammar_base) = install_mock_grammar();
        let index = TsIndex::build_with_grammar_dir(&[], &grammar_base).expect("build empty index");

        assert!(index.has_grammar_for(Path::new("test.mock")));
        assert!(!index.has_grammar_for(Path::new("test.txt")));
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_has_children() {
        let (_data_dir, grammar_base) = install_mock_grammar();

        let workspace = tempfile::tempdir().expect("workspace tempdir");
        std::fs::write(workspace.path().join("test.mock"), "fn my_method\nfn other")
            .expect("write test file");

        let index =
            TsIndex::build_with_grammar_dir(&[workspace.path().to_path_buf()], &grammar_base)
                .expect("build index");

        // "my_method" has no children (it's a leaf function)
        assert!(!index.has_children(&workspace.path().join("test.mock"), "my_method"));
    }

    /// Brace-delimited blocks: functions inside struct blocks get correct scope.
    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_brace_block_scoping() {
        let (_data_dir, grammar_base) = install_mock_grammar();

        let workspace = tempfile::tempdir().expect("workspace tempdir");
        std::fs::write(
            workspace.path().join("scoped.mock"),
            "fn callee\nfn caller {\ncallee\n}\n",
        )
        .expect("write test file");

        let index =
            TsIndex::build_with_grammar_dir(&[workspace.path().to_path_buf()], &grammar_base)
                .expect("build index");

        let results = index.query(".*", None).expect("query all");
        let names: Vec<&str> = results.iter().map(|(_, s)| s.name.as_str()).collect();
        assert!(names.contains(&"callee"), "expected callee: {names:?}");
        assert!(names.contains(&"caller"), "expected caller: {names:?}");

        let caller = results
            .iter()
            .find(|(_, s)| s.name == "caller")
            .expect("caller");
        assert_eq!(caller.1.line, 1, "caller should be at line 1");
        // caller with block should span multiple lines
        assert!(
            caller.1.end_line > caller.1.line,
            "caller block should span multiple lines: end_line={}",
            caller.1.end_line
        );
    }

    /// Nested definition: function inside struct block gets scope.
    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_nested_definition_scope() {
        let (_data_dir, grammar_base) = install_mock_grammar();

        let workspace = tempfile::tempdir().expect("workspace tempdir");
        std::fs::write(
            workspace.path().join("nested.mock"),
            "struct Outer {\nfn inner\n}\n",
        )
        .expect("write test file");

        let index =
            TsIndex::build_with_grammar_dir(&[workspace.path().to_path_buf()], &grammar_base)
                .expect("build index");

        let results = index.query(".*", None).expect("query all");
        let all_names: Vec<(&str, &str, u32, u32)> = results
            .iter()
            .map(|(_, s)| (s.name.as_str(), s.kind.as_str(), s.line, s.end_line))
            .collect();

        // Verify Outer struct spans the full block (line 0 to line 2)
        let outer = results
            .iter()
            .find(|(_, s)| s.name == "Outer")
            .expect("Outer");
        assert_eq!(outer.1.kind, "struct");
        assert_eq!(outer.1.line, 0, "Outer should start at line 0");
        assert!(
            outer.1.end_line >= 2,
            "Outer should span to at least line 2 (closing brace), got end_line={}",
            outer.1.end_line
        );

        assert!(
            results.iter().any(|(_, s)| s.name == "inner"),
            "inner not found, all symbols: {all_names:?}"
        );
        let inner = results
            .iter()
            .find(|(_, s)| s.name == "inner")
            .expect("inner");
        assert_eq!(inner.1.line, 1, "inner should be at line 1");
        assert_eq!(
            inner.1.scope.as_deref(),
            Some("Outer"),
            "inner should be scoped by Outer"
        );
        assert_eq!(
            inner.1.scope_kind.as_deref(),
            Some("struct"),
            "scope kind should be struct"
        );
    }

    /// Reproduce the exact content from the `tier1_enriched` integration test.
    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_brace_block_with_reference() {
        let (_data_dir, grammar_base) = install_mock_grammar();

        let workspace = tempfile::tempdir().expect("workspace tempdir");
        // Same content as test_grep_tier1_enriched
        std::fs::write(
            workspace.path().join("enrich.mock"),
            "fn callee_t1\nfn caller_t1 {\ncallee_t1\n}\n",
        )
        .expect("write test file");

        let index =
            TsIndex::build_with_grammar_dir(&[workspace.path().to_path_buf()], &grammar_base)
                .expect("build index");

        let results = index.query("caller_t1", None).expect("query");
        assert!(
            !results.is_empty(),
            "expected caller_t1 in index, got empty"
        );
        let (_, sym) = &results[0];
        assert_eq!(sym.name, "caller_t1");
        assert_eq!(sym.kind, "function");
        assert_eq!(sym.line, 1, "caller_t1 should be at line 1");
    }

    // --- Query and update tests ---

    /// Build a `TsIndex` with the mock grammar over a workspace directory.
    ///
    /// Returns `(TsIndex, _tempdir)` — the tempdir must stay alive.
    #[allow(clippy::expect_used, reason = "test setup")]
    fn build_test_index(workspace: &std::path::Path) -> (TsIndex, tempfile::TempDir) {
        let (data_dir, grammar_base) = install_mock_grammar();
        let index = TsIndex::build_with_grammar_dir(&[workspace.to_path_buf()], &grammar_base)
            .expect("build index");
        (index, data_dir)
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_query_regex_filter() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        std::fs::write(
            workspace.path().join("test.mock"),
            "fn foo\nfn foobar\nfn baz",
        )
        .expect("write test file");

        let (index, _data_dir) = build_test_index(workspace.path());

        let results = index.query("foo", None).expect("query");
        let names: Vec<&str> = results.iter().map(|(_, s)| s.name.as_str()).collect();

        assert!(names.contains(&"foo"), "should contain foo");
        assert!(names.contains(&"foobar"), "should contain foobar");
        assert!(!names.contains(&"baz"), "should not contain baz");
        assert_eq!(results.len(), 2);
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_query_file_scoping() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        std::fs::write(workspace.path().join("a.mock"), "fn alpha\nfn beta").expect("write a.mock");
        std::fs::write(workspace.path().join("b.mock"), "fn gamma\nfn delta")
            .expect("write b.mock");

        let (index, _data_dir) = build_test_index(workspace.path());

        let a_path = workspace.path().join("a.mock");
        let results = index
            .query(".*", Some(std::slice::from_ref(&a_path)))
            .expect("scoped query");

        for (path, _) in &results {
            assert_eq!(*path, a_path, "all results should be from a.mock");
        }
        let names: Vec<&str> = results.iter().map(|(_, s)| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
        assert!(!names.contains(&"gamma"));
        assert!(!names.contains(&"delta"));
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_update_file() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let file_path = workspace.path().join("test.mock");
        std::fs::write(&file_path, "fn foo\nfn bar").expect("write test file");

        let (index, _data_dir) = build_test_index(workspace.path());

        let initial = index.query(".*", None).expect("initial query");
        assert_eq!(initial.len(), 2);

        std::fs::write(&file_path, "fn foo\nfn bar\nfn newone").expect("rewrite test file");
        index.update_file(&file_path).expect("update_file");

        let updated = index.query(".*", None).expect("updated query");
        let names: Vec<&str> = updated.iter().map(|(_, s)| s.name.as_str()).collect();
        assert_eq!(updated.len(), 3, "should have 3 symbols after update");
        assert!(names.contains(&"newone"), "should contain new symbol");
        assert_eq!(
            names.iter().filter(|&&n| n == "foo").count(),
            1,
            "foo should appear exactly once"
        );
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_query_does_not_auto_refresh() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let file_path = workspace.path().join("test.mock");
        std::fs::write(&file_path, "fn original").expect("write test file");

        let (index, _data_dir) = build_test_index(workspace.path());

        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&file_path, "fn original\nfn added").expect("rewrite test file");

        let results = index.query("added", None).expect("query");
        assert!(
            results.is_empty(),
            "query should not auto-refresh stale files"
        );

        index.update_file(&file_path).expect("update_file");
        let results = index.query("added", None).expect("query after update");
        assert_eq!(results.len(), 1, "should find symbol after explicit update");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_query_empty_result() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        std::fs::write(workspace.path().join("test.mock"), "fn foo").expect("write test file");

        let (index, _data_dir) = build_test_index(workspace.path());

        let results = index
            .query("zzz_no_match", None)
            .expect("empty result query");
        assert!(results.is_empty(), "should return empty vec");
    }
}

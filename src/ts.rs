// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Tree-sitter index for workspace-wide symbol extraction.
//!
//! Provides [`TsIndex`], a SQLite-backed symbol index built from tree-sitter
//! grammars. The index walks workspace files, parses them using installed
//! grammars, and writes extracted symbols to the database. The write side
//! (`build`) lives here; the read/update side (`query`, `update_file`) is
//! added by ticket 02b.

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

/// Workspace-wide tree-sitter index backed by `SQLite`.
///
/// Owns its own WAL-mode database connection, separate from the session's
/// event-writing connection. Concurrent grep calls each open their own
/// read transaction via WAL mode.
pub struct TsIndex {
    /// Owned connection for symbol queries and writes.
    conn: Connection,
    /// Loaded grammars keyed by scope (e.g. `"source.rust"`).
    #[allow(dead_code, reason = "used by query/update in ticket 02b")]
    grammars: HashMap<String, tree_sitter::Language>,
    /// Scope → file extensions, from `grammars.file_types`.
    extensions: HashMap<String, Vec<String>>,
    /// tags.scm queries keyed by scope.
    #[allow(dead_code, reason = "used by update_file in ticket 02b")]
    tag_queries: HashMap<String, tree_sitter::Query>,
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

impl TsIndex {
    /// Build the tree-sitter index from workspace roots.
    ///
    /// Loads all installed grammars from the database, walks the workspace
    /// roots to find files matching grammar file types, parses each file,
    /// extracts symbols, and writes them to `SQLite` in a single transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if grammar loading fails, the database query fails,
    /// or the write transaction fails.
    #[allow(clippy::too_many_lines, reason = "sequential pipeline steps")]
    pub fn build(roots: &[PathBuf], conn: Connection) -> Result<Self> {
        let mut grammars = HashMap::new();
        let mut extensions = HashMap::new();
        let mut tag_queries = HashMap::new();
        let mut ext_to_scope: HashMap<String, String> = HashMap::new();

        // Step 1: Load installed grammars from the database.
        {
            let mut stmt = conn
                .prepare("SELECT scope, file_types, lib_path, tags_path FROM grammars")
                .context("failed to query grammars")?;

            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .context("failed to iterate grammars")?;

            for row in rows {
                let (scope, file_types_json, lib_path, tags_path) =
                    row.context("failed to read grammar row")?;

                // Derive symbol name: last `.`-component → tree_sitter_{lang}
                let lang_name = scope
                    .rsplit('.')
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("invalid scope: {scope}"))?;
                let symbol_name = format!("tree_sitter_{lang_name}");

                let language = catenary_ts::load_grammar(Path::new(&lib_path), &symbol_name)
                    .with_context(|| format!("failed to load grammar for {scope}"))?;

                let tags_source = std::fs::read_to_string(&tags_path)
                    .with_context(|| format!("failed to read tags.scm for {scope}"))?;
                let query = tree_sitter::Query::new(&language, &tags_source)
                    .map_err(|e| anyhow::anyhow!("failed to compile tags.scm for {scope}: {e}"))?;

                let file_exts: Vec<String> = serde_json::from_str(&file_types_json)
                    .with_context(|| format!("failed to parse file_types for {scope}"))?;

                for ext in &file_exts {
                    ext_to_scope.insert(ext.clone(), scope.clone());
                }

                grammars.insert(scope.clone(), language);
                extensions.insert(scope.clone(), file_exts);
                tag_queries.insert(scope, query);
            }
        }

        // Step 2: Walk workspace roots and collect symbols.
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

                let mtime_ns = std::fs::metadata(path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0i64, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));

                let mut parser = tree_sitter::Parser::new();
                parser
                    .set_language(language)
                    .map_err(|e| anyhow::anyhow!("failed to set parser language: {e}"))?;

                let Some(tree) = parser.parse(&source, None) else {
                    continue;
                };

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

                    let (scope_name, scope_kind) =
                        scope_stack.last().map_or((None, None), |&(n, k, _)| {
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

                let path_str = path.to_string_lossy().to_string();
                file_states.push((path_str.clone(), mtime_ns, scope.clone()));
                all_symbols.push((path_str, symbols));
            }
        }

        // Step 3: Write to SQLite in a single transaction.
        {
            let tx = conn.unchecked_transaction().context("begin transaction")?;

            tx.execute("DELETE FROM symbols", [])
                .context("failed to clear symbols")?;
            tx.execute("DELETE FROM file_parse_state", [])
                .context("failed to clear file_parse_state")?;

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
        })
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

    use rusqlite::Connection;

    use super::{EnrichmentCategory, TsIndex, categorize, format_ts_kind, lsp_kind_label};
    use crate::db;

    fn fixture_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test_assets")
            .join("mock_grammar")
    }

    #[allow(clippy::expect_used, reason = "test setup")]
    fn test_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let conn =
            db::open_and_migrate_at(&dir.path().join("test.db")).expect("failed to create test db");
        (dir, conn)
    }

    /// Installs the mock grammar fixture into the given output directory
    /// and registers it in the database.
    #[allow(clippy::expect_used, reason = "test setup")]
    fn install_mock_grammar(conn: &Connection, grammar_dir: &Path) {
        crate::install::install_from_dir(
            &fixture_dir(),
            grammar_dir,
            conn,
            "https://github.com/test/mock",
        )
        .expect("install mock grammar");
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
        let (db_dir, setup_conn) = test_db();
        let grammar_dir = tempfile::tempdir().expect("grammar tempdir");
        install_mock_grammar(&setup_conn, grammar_dir.path());

        let workspace = tempfile::tempdir().expect("workspace tempdir");
        std::fs::write(
            workspace.path().join("test.mock"),
            "fn foo\nstruct Bar\nfn baz",
        )
        .expect("write test file");

        let db_path = db_dir.path().join("test.db");
        let index_conn = db::open_and_migrate_at(&db_path).expect("index conn");
        let _index =
            TsIndex::build(&[workspace.path().to_path_buf()], index_conn).expect("build index");

        // Verify symbols through the setup connection (WAL read).
        let mut stmt = setup_conn
            .prepare(
                "SELECT name, kind, line, end_line, scope FROM symbols \
                 ORDER BY line",
            )
            .expect("prepare query");
        let symbols: Vec<(String, String, i64, i64, Option<String>)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .expect("query symbols")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect symbols");

        assert_eq!(symbols.len(), 3, "expected 3 symbols");

        assert_eq!(symbols[0].0, "foo");
        assert_eq!(symbols[0].1, "function");
        assert_eq!(symbols[0].2, 0);
        assert_eq!(symbols[0].3, 0);
        assert!(symbols[0].4.is_none());

        assert_eq!(symbols[1].0, "Bar");
        assert_eq!(symbols[1].1, "struct");
        assert_eq!(symbols[1].2, 1);
        assert_eq!(symbols[1].3, 1);
        assert!(symbols[1].4.is_none());

        assert_eq!(symbols[2].0, "baz");
        assert_eq!(symbols[2].1, "function");
        assert_eq!(symbols[2].2, 2);
        assert_eq!(symbols[2].3, 2);
        assert!(symbols[2].4.is_none());
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_build_index_records_mtime() {
        let (db_dir, setup_conn) = test_db();
        let grammar_dir = tempfile::tempdir().expect("grammar tempdir");
        install_mock_grammar(&setup_conn, grammar_dir.path());

        let workspace = tempfile::tempdir().expect("workspace tempdir");
        std::fs::write(
            workspace.path().join("test.mock"),
            "fn foo\nstruct Bar\nfn baz",
        )
        .expect("write test file");

        let db_path = db_dir.path().join("test.db");
        let index_conn = db::open_and_migrate_at(&db_path).expect("index conn");
        let _index =
            TsIndex::build(&[workspace.path().to_path_buf()], index_conn).expect("build index");

        let (mtime_ns, grammar): (i64, String) = setup_conn
            .query_row(
                "SELECT mtime_ns, grammar FROM file_parse_state LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("query file_parse_state");

        assert!(mtime_ns > 0, "mtime should be non-zero");
        assert_eq!(grammar, "source.mock");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_no_grammar_skip() {
        let (db_dir, setup_conn) = test_db();
        let grammar_dir = tempfile::tempdir().expect("grammar tempdir");
        install_mock_grammar(&setup_conn, grammar_dir.path());

        let workspace = tempfile::tempdir().expect("workspace tempdir");
        std::fs::write(workspace.path().join("readme.txt"), "hello world").expect("write txt file");

        let db_path = db_dir.path().join("test.db");
        let index_conn = db::open_and_migrate_at(&db_path).expect("index conn");
        let _index =
            TsIndex::build(&[workspace.path().to_path_buf()], index_conn).expect("build index");

        let count: i64 = setup_conn
            .query_row("SELECT COUNT(*) FROM file_parse_state", [], |row| {
                row.get(0)
            })
            .expect("count file_parse_state");
        assert_eq!(count, 0, "no file_parse_state for .txt files");

        let sym_count: i64 = setup_conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |row| row.get(0))
            .expect("count symbols");
        assert_eq!(sym_count, 0, "no symbols for .txt files");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_has_grammar_for() {
        let (db_dir, setup_conn) = test_db();
        let grammar_dir = tempfile::tempdir().expect("grammar tempdir");
        install_mock_grammar(&setup_conn, grammar_dir.path());

        let db_path = db_dir.path().join("test.db");
        let index_conn = db::open_and_migrate_at(&db_path).expect("index conn");
        let index = TsIndex::build(&[], index_conn).expect("build empty index");

        assert!(index.has_grammar_for(Path::new("test.mock")));
        assert!(!index.has_grammar_for(Path::new("test.txt")));
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_has_children() {
        let (db_dir, setup_conn) = test_db();

        let db_path = db_dir.path().join("test.db");
        let index_conn = db::open_and_migrate_at(&db_path).expect("index conn");
        let index = TsIndex::build(&[], index_conn).expect("build empty index");

        // Manually insert symbols through the setup connection.
        setup_conn
            .execute(
                "INSERT INTO symbols \
                 (file_path, name, kind, line, end_line, scope, scope_kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "src/test.rs",
                    "MyStruct",
                    "struct",
                    0,
                    10,
                    None::<String>,
                    None::<String>
                ],
            )
            .expect("insert struct");

        setup_conn
            .execute(
                "INSERT INTO symbols \
                 (file_path, name, kind, line, end_line, scope, scope_kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "src/test.rs",
                    "my_method",
                    "method",
                    2,
                    5,
                    "MyStruct",
                    "struct"
                ],
            )
            .expect("insert method");

        assert!(index.has_children(Path::new("src/test.rs"), "MyStruct"));
        assert!(!index.has_children(Path::new("src/test.rs"), "NoSuchScope"));
    }
}

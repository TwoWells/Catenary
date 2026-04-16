// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Single authority for file classification.
//!
//! [`FilesystemManager`] centralises binary detection, line counting, and
//! language identification (extension, filename, and shebang) behind one
//! cache keyed by path + mtime. Replaces the former `FilesystemCache`.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::lsp::glob::{FileChange, FileChangeType};

/// Files above this size are assumed binary without reading.
const BINARY_SIZE_THRESHOLD: u64 = 10 * 1024 * 1024; // 10 MB

/// File classification result.
#[derive(Debug, Clone)]
pub struct FileInfo {
    /// File modification time (seconds since epoch).
    pub mtime: u64,
    /// File size in bytes.
    pub size: u64,
    /// Owning workspace root (longest-prefix match), or `None` if outside
    /// all known roots. Resolved live on every [`FilesystemManager::classify`]
    /// call — not cached.
    pub root: Option<PathBuf>,
    /// File kind (binary or text with metadata).
    pub kind: FileKind,
}

impl FileInfo {
    /// Returns the LSP language identifier, if detectable.
    #[must_use]
    pub const fn language_id(&self) -> Option<&'static str> {
        match self.kind {
            FileKind::Text { language_id, .. } => language_id,
            FileKind::Binary | FileKind::Folder => None,
        }
    }
}

/// File classification: binary, text, or folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// Binary file (contains null bytes or exceeds size threshold).
    Binary,
    /// Text file with line count and optional language ID.
    Text {
        /// Number of lines (newline-delimited).
        lines: usize,
        /// LSP language identifier, if detectable. `None` for files with
        /// no known extension, filename, or shebang.
        language_id: Option<&'static str>,
    },
    /// Directory entry. Used by [`FilesystemManager::seed`] and
    /// [`FilesystemManager::diff`] for tracking directory creation and
    /// deletion.
    Folder,
}

/// Cross-tool filesystem classification cache.
///
/// Single authority for file metadata: binary detection, line count,
/// language ID, and shebang detection. Shared by `GrepServer` and
/// `GlobServer` through `Toolbox`.
///
/// Also owns the workspace root list for longest-prefix root resolution.
pub struct FilesystemManager {
    cache: std::sync::Mutex<HashMap<PathBuf, CachedEntry>>,
    roots: std::sync::Mutex<Vec<PathBuf>>,
}

/// Cache entry storing classification results keyed by mtime.
///
/// `kind` is `None` for seed-only entries (stat-only, no classification).
/// [`FilesystemManager::classify`] overwrites these on first access.
struct CachedEntry {
    mtime: u64,
    kind: Option<FileKind>,
}

impl Default for FilesystemManager {
    fn default() -> Self {
        Self {
            cache: std::sync::Mutex::new(HashMap::new()),
            roots: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl FilesystemManager {
    /// Creates an empty manager.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Classifies a file, using the cache when possible.
    ///
    /// Returns a [`FileInfo`] with binary/text classification, line count,
    /// and language ID. Cache is keyed by absolute path + mtime. On mtime
    /// change the entry is re-scanned.
    pub fn classify(&self, path: &Path, metadata: &std::fs::Metadata) -> FileInfo {
        let mtime = mtime_secs(metadata);
        let size = metadata.len();
        let root = self.resolve_root(path);

        // Check cache — skip unclassified (seed-only) entries.
        if let Ok(cache) = self.cache.lock()
            && let Some(entry) = cache.get(path)
            && entry.mtime == mtime
            && let Some(kind) = entry.kind
        {
            return FileInfo {
                mtime,
                size,
                root,
                kind,
            };
        }

        // Extension/filename detection (pure, no I/O)
        let ext_language = detect_language_id_opt(path);

        // Scan file for binary/text + line count + shebang
        let kind = scan_file(path, metadata).map_or(FileKind::Binary, |scan| {
            let language_id = ext_language.or(scan.shebang_language);
            FileKind::Text {
                lines: scan.lines,
                language_id,
            }
        });

        // Update cache
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(
                path.to_path_buf(),
                CachedEntry {
                    mtime,
                    kind: Some(kind),
                },
            );
        }

        FileInfo {
            mtime,
            size,
            root,
            kind,
        }
    }

    /// Returns `true` if the file is binary, using the cache when possible.
    pub fn is_binary(&self, path: &Path, metadata: &std::fs::Metadata) -> bool {
        matches!(self.classify(path, metadata).kind, FileKind::Binary)
    }

    /// Returns the line count if the file is text, or `None` if binary or folder.
    pub fn line_count(&self, path: &Path, metadata: &std::fs::Metadata) -> Option<usize> {
        match self.classify(path, metadata).kind {
            FileKind::Binary | FileKind::Folder => None,
            FileKind::Text { lines, .. } => Some(lines),
        }
    }

    /// Returns the LSP language identifier for a file path, or `None` if unknown.
    ///
    /// Tries extension/filename detection first (no I/O). If that fails
    /// and the file exists on disk, falls back to full classification
    /// which includes shebang detection.
    pub fn language_id(&self, path: &Path) -> Option<&'static str> {
        // Fast path: extension/filename (no I/O)
        if let Some(lang) = detect_language_id_opt(path) {
            return Some(lang);
        }
        // Slow path: full classification for shebang
        let metadata = std::fs::metadata(path).ok()?;
        self.classify(path, &metadata).language_id()
    }

    /// Resolves the owning workspace root for a path.
    ///
    /// Returns the longest-prefix match against known roots, or `None` if
    /// the path is outside all known roots.
    #[must_use]
    pub fn resolve_root(&self, path: &Path) -> Option<PathBuf> {
        let Ok(roots) = self.roots.lock() else {
            return None;
        };
        roots
            .iter()
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.as_os_str().len())
            .cloned()
    }

    /// Updates the known workspace root set.
    pub fn set_roots(&self, roots: Vec<PathBuf>) {
        if let Ok(mut current) = self.roots.lock() {
            *current = roots;
        }
    }

    /// Scans workspace roots and returns the set of language keys that have
    /// matching files present among `configured_keys`.
    ///
    /// Respects `.gitignore` and skips hidden files. Uses extension/filename
    /// detection first, then full classification (including shebang) for
    /// files without a recognised extension. Falls back to the raw file
    /// extension for custom languages. Exits early once all configured
    /// languages have been detected.
    #[allow(clippy::implicit_hasher, reason = "All callers use the default hasher")]
    pub fn detect_workspace_languages(
        &self,
        roots: &[PathBuf],
        configured_keys: &HashSet<&str>,
    ) -> HashSet<String> {
        let mut detected = HashSet::new();

        for root in roots {
            if !root.exists() {
                continue;
            }

            let walker = WalkBuilder::new(root).git_ignore(true).hidden(true).build();

            for entry in walker.flatten() {
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    continue;
                }

                let path = entry.path();

                // Fast path: extension/filename (no I/O beyond the walk).
                // Slow path: full classification (shebang detection).
                let lang = detect_language_id_opt(path).or_else(|| {
                    let metadata = entry.metadata().ok()?;
                    self.classify(path, &metadata).language_id()
                });

                if let Some(lang) = lang {
                    if configured_keys.contains(lang) {
                        detected.insert(lang.to_string());
                    }
                } else if let Some(ext) = path.extension().and_then(|e| e.to_str())
                    && configured_keys.contains(ext)
                {
                    detected.insert(ext.to_string());
                }

                if detected.len() == configured_keys.len() {
                    return detected;
                }
            }
        }

        detected
    }

    /// Populates the cache with `(path, mtime)` for every file and directory
    /// in the known roots. Stat-only — no content read, no classification.
    /// Respects `.gitignore` via the `ignore` crate.
    ///
    /// Called once at session start during `LspClientManager` init. Subsequent
    /// root additions use `add_root` which already walks the new root.
    pub fn seed(&self) {
        let roots = {
            let Ok(roots) = self.roots.lock() else {
                return;
            };
            roots.clone()
        };

        let mut entries = HashMap::new();
        for root in &roots {
            if !root.exists() {
                continue;
            }
            let walker = WalkBuilder::new(root).git_ignore(true).hidden(true).build();
            for entry in walker.flatten() {
                let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
                let path = entry.into_path();
                if let Ok(meta) = std::fs::metadata(&path) {
                    let kind = if is_dir { Some(FileKind::Folder) } else { None };
                    entries.insert(
                        path,
                        CachedEntry {
                            mtime: mtime_secs(&meta),
                            kind,
                        },
                    );
                }
            }
        }

        if let Ok(mut cache) = self.cache.lock() {
            *cache = entries;
        }
    }

    /// Diffs current disk state against the cache.
    ///
    /// Returns creates, changes, and deletes since last diff (or since seed
    /// for the first call). Updates the cache to reflect current disk state.
    ///
    /// The cache lock is held for the full duration (walk + compare + update).
    /// This is acceptable because `diff()` runs synchronously at tool
    /// boundaries, the critical section is stat-bound, and no other code path
    /// contends on the cache lock for long.
    pub fn diff(&self) -> Vec<FileChange> {
        let Ok(roots) = self.roots.lock() else {
            return Vec::new();
        };
        let Ok(mut cache) = self.cache.lock() else {
            return Vec::new();
        };

        // Walk all roots, collecting current (path, mtime, is_dir) tuples.
        let mut current: HashMap<PathBuf, (u64, bool)> = HashMap::new();
        for root in roots.iter() {
            if !root.exists() {
                continue;
            }
            let walker = WalkBuilder::new(root).git_ignore(true).hidden(true).build();
            for entry in walker.flatten() {
                let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
                let path = entry.into_path();
                if let Ok(meta) = std::fs::metadata(&path) {
                    current.insert(path, (mtime_secs(&meta), is_dir));
                }
            }
        }
        drop(roots);

        let mut changes = Vec::new();

        // Detect created and changed.
        for (path, (mtime, _)) in &current {
            match cache.get(path) {
                None => changes.push(FileChange {
                    path: path.clone(),
                    change_type: FileChangeType::Created,
                }),
                Some(entry) if entry.mtime != *mtime => changes.push(FileChange {
                    path: path.clone(),
                    change_type: FileChangeType::Changed,
                }),
                _ => {}
            }
        }

        // Detect deleted.
        let deleted: Vec<PathBuf> = cache
            .keys()
            .filter(|p| !current.contains_key(*p))
            .cloned()
            .collect();
        for path in &deleted {
            changes.push(FileChange {
                path: path.clone(),
                change_type: FileChangeType::Deleted,
            });
            cache.remove(path);
        }

        // Update cache for created and changed entries.
        for change in &changes {
            match change.change_type {
                FileChangeType::Created => {
                    if let Some(&(mtime, is_dir)) = current.get(&change.path) {
                        let kind = if is_dir { Some(FileKind::Folder) } else { None };
                        cache.insert(change.path.clone(), CachedEntry { mtime, kind });
                    }
                }
                FileChangeType::Changed => {
                    if let Some(entry) = cache.get_mut(&change.path)
                        && let Some(&(mtime, is_dir)) = current.get(&change.path)
                    {
                        entry.mtime = mtime;
                        entry.kind = if is_dir { Some(FileKind::Folder) } else { None };
                    }
                }
                FileChangeType::Deleted => {} // Already removed above.
            }
        }

        changes
    }

    /// Refreshes cache entries for specific paths.
    ///
    /// Re-stats each path and updates its mtime in the cache. If a path no
    /// longer exists, removes it from the cache.
    ///
    /// Used by `done_editing` to prevent the next [`diff`](Self::diff) from
    /// reporting edited files as `Changed`.
    pub fn mark_current(&self, paths: &[PathBuf]) {
        let Ok(mut cache) = self.cache.lock() else {
            return;
        };
        for path in paths {
            match std::fs::metadata(path) {
                Ok(meta) => {
                    let mtime = mtime_secs(&meta);
                    if let Some(entry) = cache.get_mut(path) {
                        entry.mtime = mtime;
                    } else {
                        cache.insert(path.clone(), CachedEntry { mtime, kind: None });
                    }
                }
                Err(_) => {
                    cache.remove(path);
                }
            }
        }
    }
}

/// Formats a file size in human-readable form.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "display-only rounding is acceptable"
)]
pub fn format_file_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Extension/filename detection — returns `None` for unrecognised files.
pub(crate) fn detect_language_id_opt(path: &Path) -> Option<&'static str> {
    // Filename-based detection (exact match).
    if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
        let lang = match file_name {
            "Dockerfile" => "dockerfile",
            "Makefile" | "GNUmakefile" => "makefile",
            "CMakeLists.txt" => "cmake",
            "Cargo.toml" | "Cargo.lock" => "toml",
            "Gemfile" | "Rakefile" => "ruby",
            "Justfile" | "justfile" => "just",
            "PKGBUILD" => "shellscript",
            _ => "",
        };
        if !lang.is_empty() {
            return Some(lang);
        }
    }

    // Extension-based detection.
    match path.extension().and_then(|e| e.to_str()) {
        // Systems
        Some("rs") => Some("rust"),
        Some("go") => Some("go"),
        Some("c") => Some("c"),
        Some("cpp" | "cc" | "cxx" | "h" | "hpp") => Some("cpp"),
        Some("zig") => Some("zig"),
        Some("d") => Some("d"),
        Some("v") => Some("v"),
        Some("nim") => Some("nim"),

        // JVM
        Some("java") => Some("java"),
        Some("kt" | "kts") => Some("kotlin"),
        Some("scala" | "sc") => Some("scala"),
        Some("groovy" | "gvy") => Some("groovy"),
        Some("clj" | "cljs" | "cljc") => Some("clojure"),

        // .NET
        Some("cs") => Some("csharp"),
        Some("fs" | "fsx" | "fsi") => Some("fsharp"),

        // Apple
        Some("swift") => Some("swift"),
        Some("m" | "mm") => Some("objective-c"),

        // Scripting
        Some("py") => Some("python"),
        Some("rb") => Some("ruby"),
        Some("pl" | "pm") => Some("perl"),
        Some("php") => Some("php"),
        Some("lua") => Some("lua"),
        Some("tcl") => Some("tcl"),
        Some("cr") => Some("crystal"),

        // JavaScript / TypeScript
        Some("js" | "mjs" | "cjs") => Some("javascript"),
        Some("ts" | "mts" | "cts") => Some("typescript"),
        Some("tsx") => Some("typescriptreact"),
        Some("jsx") => Some("javascriptreact"),

        // Functional
        Some("hs" | "lhs") => Some("haskell"),
        Some("ml" | "mli") => Some("ocaml"),
        Some("elm") => Some("elm"),
        Some("gleam") => Some("gleam"),
        Some("ex" | "exs") => Some("elixir"),
        Some("erl" | "hrl") => Some("erlang"),
        Some("purs") => Some("purescript"),

        // Shell
        Some("sh" | "bash" | "zsh" | "ebuild" | "eclass" | "install") => Some("shellscript"),
        Some("fish") => Some("fish"),
        Some("ps1" | "psm1" | "psd1") => Some("powershell"),

        // Data science
        Some("r" | "R") => Some("r"),
        Some("jl") => Some("julia"),
        Some("mojo") => Some("mojo"),

        // Web frontend
        Some("html" | "htm") => Some("html"),
        Some("css") => Some("css"),
        Some("scss") => Some("scss"),
        Some("sass") => Some("sass"),
        Some("less") => Some("less"),
        Some("svelte") => Some("svelte"),
        Some("vue") => Some("vue"),

        // Data / config
        Some("json" | "jsonc") => Some("json"),
        Some("yaml" | "yml") => Some("yaml"),
        Some("toml") => Some("toml"),
        Some("xml" | "xsl" | "xslt" | "xsd") => Some("xml"),
        Some("sql") => Some("sql"),
        Some("graphql" | "gql") => Some("graphql"),
        Some("proto") => Some("proto"),

        // Markup / docs
        Some("md" | "mdx") => Some("markdown"),
        Some("rst") => Some("restructuredtext"),
        Some("tex" | "latex") => Some("latex"),
        Some("typ") => Some("typst"),

        // Infrastructure / config languages
        Some("nix") => Some("nix"),
        Some("tf" | "tfvars") => Some("terraform"),
        Some("cmake") => Some("cmake"),
        Some("dart") => Some("dart"),
        Some("dockerfile") => Some("dockerfile"),

        _ => None,
    }
}

/// Extracts mtime as seconds since epoch (cross-platform).
fn mtime_secs(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// Intermediate result from a single-pass file scan.
struct ScanResult {
    lines: usize,
    shebang_language: Option<&'static str>,
}

/// Scans a file for null bytes, counts lines, and extracts shebang in one pass.
///
/// Returns `Some(ScanResult)` for text files, `None` for binary files.
/// Files above the size threshold are assumed binary without reading.
fn scan_file(path: &Path, metadata: &std::fs::Metadata) -> Option<ScanResult> {
    if metadata.len() > BINARY_SIZE_THRESHOLD {
        return None;
    }

    let Ok(file) = std::fs::File::open(path) else {
        return Some(ScanResult {
            lines: 0,
            shebang_language: None,
        });
    };

    let mut reader = std::io::BufReader::new(file);
    let mut buf = [0u8; 8192];
    let mut lines = 0;
    let mut shebang_language = None;
    let mut first_chunk = true;

    loop {
        let Ok(n) = reader.read(&mut buf) else {
            return Some(ScanResult {
                lines,
                shebang_language,
            });
        };
        if n == 0 {
            return Some(ScanResult {
                lines,
                shebang_language,
            });
        }
        if memchr::memchr(0, &buf[..n]).is_some() {
            return None; // Binary
        }

        if first_chunk {
            first_chunk = false;
            let first_line_end = memchr::memchr(b'\n', &buf[..n]).unwrap_or(n);
            shebang_language = parse_shebang(&buf[..first_line_end]);
        }

        lines += memchr::memchr_iter(b'\n', &buf[..n]).count();
    }
}

/// Parses a shebang line and returns the corresponding LSP language ID.
///
/// Handles both direct paths (`#!/bin/bash`) and `env` indirection
/// (`#!/usr/bin/env bash`). Flags after the interpreter are ignored.
fn parse_shebang(first_line: &[u8]) -> Option<&'static str> {
    let line = first_line.strip_prefix(b"#!")?;
    let line = line.trim_ascii_start();
    let line_str = std::str::from_utf8(line).ok()?;

    let mut parts = line_str.split_whitespace();
    let command = parts.next()?;

    // If command is /usr/bin/env (or similar), the interpreter is the next
    // non-flag argument.
    let interpreter = if command.ends_with("/env") {
        parts.find(|p| !p.starts_with('-'))?
    } else {
        command
    };

    let basename = interpreter.rsplit('/').next()?;

    match basename {
        "bash" | "sh" | "zsh" | "dash" | "ksh" => Some("shellscript"),
        "fish" => Some("fish"),
        "python" | "python3" | "python2" => Some("python"),
        "node" | "nodejs" => Some("javascript"),
        "deno" => Some("typescript"),
        "ruby" | "irb" => Some("ruby"),
        "perl" => Some("perl"),
        "php" => Some("php"),
        "lua" | "luajit" => Some("lua"),
        "tclsh" | "wish" => Some("tcl"),
        "Rscript" => Some("r"),
        "julia" => Some("julia"),
        "elixir" | "iex" => Some("elixir"),
        "erl" => Some("erlang"),
        "swift" => Some("swift"),
        "kotlin" => Some("kotlin"),
        "scala" => Some("scala"),
        "groovy" => Some("groovy"),
        "crystal" => Some("crystal"),
        _ => None,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::io::Write;

    // --- Classification (migrated from FilesystemCache) ---

    #[test]
    fn classify_binary_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("binary.bin");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(&[0x89, 0x50, 0x4E, 0x47, 0x00, 0x0A])
            .expect("write");
        drop(f);

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(mgr.classify(&path, &metadata).kind, FileKind::Binary);
    }

    #[test]
    fn classify_text_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("text.txt");
        std::fs::write(&path, "Hello, world!\nLine two.\n").expect("write");

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(
            mgr.classify(&path, &metadata).kind,
            FileKind::Text {
                lines: 2,
                language_id: None,
            }
        );
    }

    #[test]
    fn classify_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").expect("write");

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(
            mgr.classify(&path, &metadata).kind,
            FileKind::Text {
                lines: 0,
                language_id: None,
            }
        );
    }

    #[test]
    fn line_count_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("code.rs");
        std::fs::write(&path, "fn main() {\n    println!(\"hi\");\n}\n").expect("write");

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");

        // First call: scan + cache
        assert_eq!(mgr.line_count(&path, &metadata), Some(3));
        // Second call: cache hit (line count is now cached)
        assert_eq!(mgr.line_count(&path, &metadata), Some(3));
    }

    #[test]
    fn line_count_binary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("image.png");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(&[0x89, 0x50, 0x4E, 0x47, 0x00]).expect("write");
        drop(f);

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(mgr.line_count(&path, &metadata), None);
    }

    #[test]
    fn cache_populated_by_classify() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cached.bin");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(&[0x00, 0x01, 0x02]).expect("write");
        drop(f);

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");

        assert!(mgr.is_binary(&path, &metadata));
        assert!(mgr.is_binary(&path, &metadata));

        let len = mgr.cache.lock().expect("lock").len();
        assert_eq!(len, 1);
    }

    // --- Language detection (migrated from document_manager) ---

    #[test]
    fn language_detection_filenames() {
        assert_eq!(
            detect_language_id_opt(Path::new("Dockerfile")),
            Some("dockerfile")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("Makefile")),
            Some("makefile")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("GNUmakefile")),
            Some("makefile")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("CMakeLists.txt")),
            Some("cmake")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("Cargo.toml")),
            Some("toml")
        );
        assert_eq!(detect_language_id_opt(Path::new("Gemfile")), Some("ruby"));
        assert_eq!(detect_language_id_opt(Path::new("Rakefile")), Some("ruby"));
        assert_eq!(detect_language_id_opt(Path::new("Justfile")), Some("just"));
        assert_eq!(
            detect_language_id_opt(Path::new("PKGBUILD")),
            Some("shellscript")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines, reason = "exhaustive extension coverage")]
    fn language_detection_extensions() {
        // Systems
        assert_eq!(detect_language_id_opt(Path::new("test.rs")), Some("rust"));
        assert_eq!(detect_language_id_opt(Path::new("test.go")), Some("go"));
        assert_eq!(detect_language_id_opt(Path::new("test.c")), Some("c"));
        assert_eq!(detect_language_id_opt(Path::new("test.cpp")), Some("cpp"));
        assert_eq!(detect_language_id_opt(Path::new("test.h")), Some("cpp"));
        assert_eq!(detect_language_id_opt(Path::new("test.zig")), Some("zig"));
        assert_eq!(detect_language_id_opt(Path::new("test.d")), Some("d"));
        assert_eq!(detect_language_id_opt(Path::new("test.v")), Some("v"));
        assert_eq!(detect_language_id_opt(Path::new("test.nim")), Some("nim"));

        // JVM
        assert_eq!(detect_language_id_opt(Path::new("test.java")), Some("java"));
        assert_eq!(detect_language_id_opt(Path::new("test.kt")), Some("kotlin"));
        assert_eq!(
            detect_language_id_opt(Path::new("test.scala")),
            Some("scala")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.groovy")),
            Some("groovy")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.clj")),
            Some("clojure")
        );

        // .NET
        assert_eq!(detect_language_id_opt(Path::new("test.cs")), Some("csharp"));
        assert_eq!(detect_language_id_opt(Path::new("test.fs")), Some("fsharp"));

        // Apple
        assert_eq!(
            detect_language_id_opt(Path::new("test.swift")),
            Some("swift")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.m")),
            Some("objective-c")
        );

        // Scripting
        assert_eq!(detect_language_id_opt(Path::new("test.py")), Some("python"));
        assert_eq!(detect_language_id_opt(Path::new("test.rb")), Some("ruby"));
        assert_eq!(detect_language_id_opt(Path::new("test.pl")), Some("perl"));
        assert_eq!(detect_language_id_opt(Path::new("test.php")), Some("php"));
        assert_eq!(detect_language_id_opt(Path::new("test.lua")), Some("lua"));
        assert_eq!(detect_language_id_opt(Path::new("test.tcl")), Some("tcl"));
        assert_eq!(
            detect_language_id_opt(Path::new("test.cr")),
            Some("crystal")
        );

        // JavaScript / TypeScript
        assert_eq!(
            detect_language_id_opt(Path::new("test.js")),
            Some("javascript")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.mjs")),
            Some("javascript")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.ts")),
            Some("typescript")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.mts")),
            Some("typescript")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.tsx")),
            Some("typescriptreact")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.jsx")),
            Some("javascriptreact")
        );

        // Functional
        assert_eq!(
            detect_language_id_opt(Path::new("test.hs")),
            Some("haskell")
        );
        assert_eq!(detect_language_id_opt(Path::new("test.ml")), Some("ocaml"));
        assert_eq!(detect_language_id_opt(Path::new("test.elm")), Some("elm"));
        assert_eq!(
            detect_language_id_opt(Path::new("test.gleam")),
            Some("gleam")
        );
        assert_eq!(detect_language_id_opt(Path::new("test.ex")), Some("elixir"));
        assert_eq!(
            detect_language_id_opt(Path::new("test.erl")),
            Some("erlang")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.purs")),
            Some("purescript")
        );

        // Shell
        assert_eq!(
            detect_language_id_opt(Path::new("test.sh")),
            Some("shellscript")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.bash")),
            Some("shellscript")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.ebuild")),
            Some("shellscript")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.eclass")),
            Some("shellscript")
        );
        assert_eq!(detect_language_id_opt(Path::new("test.fish")), Some("fish"));
        assert_eq!(
            detect_language_id_opt(Path::new("test.ps1")),
            Some("powershell")
        );

        // Data science
        assert_eq!(detect_language_id_opt(Path::new("test.r")), Some("r"));
        assert_eq!(detect_language_id_opt(Path::new("test.jl")), Some("julia"));

        // Web frontend
        assert_eq!(detect_language_id_opt(Path::new("test.html")), Some("html"));
        assert_eq!(detect_language_id_opt(Path::new("test.css")), Some("css"));
        assert_eq!(detect_language_id_opt(Path::new("test.scss")), Some("scss"));
        assert_eq!(detect_language_id_opt(Path::new("test.less")), Some("less"));
        assert_eq!(
            detect_language_id_opt(Path::new("test.svelte")),
            Some("svelte")
        );
        assert_eq!(detect_language_id_opt(Path::new("test.vue")), Some("vue"));

        // Data / config
        assert_eq!(detect_language_id_opt(Path::new("test.json")), Some("json"));
        assert_eq!(
            detect_language_id_opt(Path::new("test.jsonc")),
            Some("json")
        );
        assert_eq!(detect_language_id_opt(Path::new("test.yaml")), Some("yaml"));
        assert_eq!(detect_language_id_opt(Path::new("test.toml")), Some("toml"));
        assert_eq!(detect_language_id_opt(Path::new("test.xml")), Some("xml"));
        assert_eq!(detect_language_id_opt(Path::new("test.sql")), Some("sql"));
        assert_eq!(
            detect_language_id_opt(Path::new("test.graphql")),
            Some("graphql")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.proto")),
            Some("proto")
        );

        // Markup / docs
        assert_eq!(
            detect_language_id_opt(Path::new("test.md")),
            Some("markdown")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.mdx")),
            Some("markdown")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.rst")),
            Some("restructuredtext")
        );
        assert_eq!(detect_language_id_opt(Path::new("test.tex")), Some("latex"));
        assert_eq!(detect_language_id_opt(Path::new("test.typ")), Some("typst"));

        // Infrastructure
        assert_eq!(detect_language_id_opt(Path::new("test.nix")), Some("nix"));
        assert_eq!(
            detect_language_id_opt(Path::new("test.tf")),
            Some("terraform")
        );
        assert_eq!(detect_language_id_opt(Path::new("test.dart")), Some("dart"));

        // Unknown
        assert_eq!(detect_language_id_opt(Path::new("test.unknown")), None);
        assert_eq!(detect_language_id_opt(Path::new("noextension")), None);
    }

    // --- Shebang detection ---

    #[test]
    fn shebang_bash_direct() {
        assert_eq!(parse_shebang(b"#!/bin/bash"), Some("shellscript"));
    }

    #[test]
    fn shebang_bash_env() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env bash"), Some("shellscript"));
    }

    #[test]
    fn shebang_sh() {
        assert_eq!(parse_shebang(b"#!/bin/sh"), Some("shellscript"));
    }

    #[test]
    fn shebang_python_env() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env python3"), Some("python"));
    }

    #[test]
    fn shebang_python_direct() {
        assert_eq!(parse_shebang(b"#!/usr/bin/python"), Some("python"));
    }

    #[test]
    fn shebang_node() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env node"), Some("javascript"));
    }

    #[test]
    fn shebang_ruby() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env ruby"), Some("ruby"));
    }

    #[test]
    fn shebang_perl() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env perl"), Some("perl"));
    }

    #[test]
    fn shebang_php() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env php"), Some("php"));
    }

    #[test]
    fn shebang_lua() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env lua"), Some("lua"));
    }

    #[test]
    fn shebang_luajit() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env luajit"), Some("lua"));
    }

    #[test]
    fn shebang_tclsh() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env tclsh"), Some("tcl"));
    }

    #[test]
    fn shebang_rscript() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env Rscript"), Some("r"));
    }

    #[test]
    fn shebang_julia() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env julia"), Some("julia"));
    }

    #[test]
    fn shebang_elixir() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env elixir"), Some("elixir"));
    }

    #[test]
    fn shebang_swift() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env swift"), Some("swift"));
    }

    #[test]
    fn shebang_kotlin() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env kotlin"), Some("kotlin"));
    }

    #[test]
    fn shebang_groovy() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env groovy"), Some("groovy"));
    }

    #[test]
    fn shebang_crystal() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env crystal"), Some("crystal"));
    }

    #[test]
    fn shebang_deno() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env deno"), Some("typescript"));
    }

    #[test]
    fn shebang_fish() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env fish"), Some("fish"));
    }

    #[test]
    fn shebang_erl() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env erl"), Some("erlang"));
    }

    #[test]
    fn shebang_scala() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env scala"), Some("scala"));
    }

    #[test]
    fn shebang_with_flags() {
        assert_eq!(parse_shebang(b"#!/bin/bash -e"), Some("shellscript"));
    }

    #[test]
    fn shebang_space_after_hash_bang() {
        assert_eq!(parse_shebang(b"#! /bin/bash"), Some("shellscript"));
    }

    #[test]
    fn shebang_env_with_flags() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env -S python3"), Some("python"));
    }

    #[test]
    fn shebang_unknown_interpreter() {
        assert_eq!(parse_shebang(b"#!/usr/bin/env something_unknown"), None);
    }

    #[test]
    fn no_shebang() {
        assert_eq!(parse_shebang(b"hello world"), None);
    }

    // --- Integration: classify + shebang ---

    #[test]
    fn classify_extensionless_with_shebang() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("my_script");
        std::fs::write(&path, "#!/bin/bash\necho hello\n").expect("write");

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(
            mgr.classify(&path, &metadata).kind,
            FileKind::Text {
                lines: 2,
                language_id: Some("shellscript"),
            }
        );
    }

    #[test]
    fn classify_extensionless_without_shebang() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("data_file");
        std::fs::write(&path, "just some text\n").expect("write");

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(
            mgr.classify(&path, &metadata).kind,
            FileKind::Text {
                lines: 1,
                language_id: None,
            }
        );
    }

    #[test]
    fn classify_binary_skips_shebang() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fake_script");
        let mut content = b"#!/bin/bash\n".to_vec();
        content.push(0x00);
        content.extend_from_slice(b"echo hello\n");
        std::fs::write(&path, &content).expect("write");

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(mgr.classify(&path, &metadata).kind, FileKind::Binary);
    }

    // --- format_file_size ---

    #[test]
    fn format_file_size_units() {
        assert_eq!(format_file_size(0), "0 B");
        assert_eq!(format_file_size(512), "512 B");
        assert_eq!(format_file_size(1024), "1 KB");
        assert_eq!(format_file_size(1_048_576), "1.0 MB");
        assert_eq!(format_file_size(1_073_741_824), "1.0 GB");
        assert_eq!(format_file_size(5_368_709_120), "5.0 GB");
    }

    #[test]
    fn classify_extension_takes_priority_over_shebang() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("script.py");
        std::fs::write(&path, "#!/usr/bin/env ruby\nprint('hello')\n").expect("write");

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(
            mgr.classify(&path, &metadata).kind,
            FileKind::Text {
                lines: 2,
                language_id: Some("python"),
            }
        );
    }

    // --- Root resolution ---

    #[test]
    fn resolve_root_single_match() {
        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![PathBuf::from("/home/user/project")]);
        assert_eq!(
            mgr.resolve_root(Path::new("/home/user/project/src/main.rs")),
            Some(PathBuf::from("/home/user/project"))
        );
    }

    #[test]
    fn resolve_root_outside_all_roots() {
        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![PathBuf::from("/home/user/project")]);
        assert_eq!(mgr.resolve_root(Path::new("/other/path/file.rs")), None);
    }

    #[test]
    fn resolve_root_longest_prefix_wins() {
        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![
            PathBuf::from("/home/user/project"),
            PathBuf::from("/home/user/project/subdir"),
        ]);
        assert_eq!(
            mgr.resolve_root(Path::new("/home/user/project/subdir/foo.rs")),
            Some(PathBuf::from("/home/user/project/subdir"))
        );
    }

    #[test]
    fn resolve_root_no_roots() {
        let mgr = FilesystemManager::new();
        assert_eq!(mgr.resolve_root(Path::new("/any/path/file.rs")), None);
    }

    #[test]
    fn set_roots_updates_resolution() {
        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![PathBuf::from("/home/user/project")]);
        assert_eq!(
            mgr.resolve_root(Path::new("/home/user/project/src/main.rs")),
            Some(PathBuf::from("/home/user/project"))
        );

        mgr.set_roots(vec![PathBuf::from("/other/root")]);
        assert_eq!(
            mgr.resolve_root(Path::new("/home/user/project/src/main.rs")),
            None
        );
        assert_eq!(
            mgr.resolve_root(Path::new("/other/root/file.rs")),
            Some(PathBuf::from("/other/root"))
        );
    }

    #[test]
    fn classify_populates_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("code.rs");
        std::fs::write(&path, "fn main() {}\n").expect("write");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        let metadata = std::fs::metadata(&path).expect("metadata");
        let info = mgr.classify(&path, &metadata);
        assert_eq!(info.root, Some(dir.path().to_path_buf()));
    }

    #[test]
    fn classify_root_none_when_outside() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("code.rs");
        std::fs::write(&path, "fn main() {}\n").expect("write");

        let mgr = FilesystemManager::new();
        // No roots set
        let metadata = std::fs::metadata(&path).expect("metadata");
        let info = mgr.classify(&path, &metadata);
        assert_eq!(info.root, None);
    }

    // --- Seed / Diff / Mark current ---

    /// Helper: set a file's mtime to a specific epoch second.
    fn set_mtime(path: &Path, epoch_secs: u64) {
        let time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(epoch_secs);
        let times = std::fs::FileTimes::new().set_modified(time);
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open for set_mtime");
        file.set_times(times).expect("set_times");
    }

    #[test]
    fn seed_populates_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").expect("write");
        std::fs::write(dir.path().join("b.rs"), "fn b() {}\n").expect("write");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        let changes = mgr.diff();
        assert!(changes.is_empty(), "diff after seed should be empty");
    }

    #[test]
    fn diff_detects_created_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("existing.rs"), "fn e() {}\n").expect("write");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        std::fs::write(dir.path().join("new.rs"), "fn n() {}\n").expect("write");

        let changes = mgr.diff();
        assert_eq!(changes.len(), 1);
        assert!(changes[0].path.ends_with("new.rs"));
        assert_eq!(changes[0].change_type, FileChangeType::Created);
    }

    #[test]
    fn diff_detects_changed_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("code.rs");
        std::fs::write(&path, "fn original() {}\n").expect("write");
        set_mtime(&path, 1_000_000);

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        // Change content and bump mtime.
        std::fs::write(&path, "fn modified() {}\n").expect("write");
        set_mtime(&path, 2_000_000);

        let changes = mgr.diff();
        assert_eq!(changes.len(), 1);
        assert!(changes[0].path.ends_with("code.rs"));
        assert_eq!(changes[0].change_type, FileChangeType::Changed);
    }

    #[test]
    fn diff_detects_deleted_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("gone.rs");
        std::fs::write(&path, "fn gone() {}\n").expect("write");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        std::fs::remove_file(&path).expect("remove");

        let changes = mgr.diff();
        assert_eq!(changes.len(), 1);
        assert!(changes[0].path.ends_with("gone.rs"));
        assert_eq!(changes[0].change_type, FileChangeType::Deleted);
    }

    #[test]
    fn diff_detects_created_directory() {
        let dir = tempfile::tempdir().expect("tempdir");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        std::fs::create_dir(dir.path().join("subdir")).expect("mkdir");

        let changes = mgr.diff();
        assert!(
            changes
                .iter()
                .any(|c| c.path.ends_with("subdir") && c.change_type == FileChangeType::Created),
            "expected Created for new directory, got: {changes:?}",
        );
    }

    #[test]
    fn diff_detects_deleted_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("subdir")).expect("mkdir");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        std::fs::remove_dir(dir.path().join("subdir")).expect("rmdir");

        let changes = mgr.diff();
        assert!(
            changes
                .iter()
                .any(|c| c.path.ends_with("subdir") && c.change_type == FileChangeType::Deleted),
            "expected Deleted for removed directory, got: {changes:?}",
        );
    }

    #[test]
    fn diff_updates_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("file.rs");
        std::fs::write(&path, "fn f() {}\n").expect("write");
        set_mtime(&path, 1_000_000);

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        // Trigger a change.
        std::fs::write(&path, "fn changed() {}\n").expect("write");
        set_mtime(&path, 2_000_000);

        let first = mgr.diff();
        assert_eq!(first.len(), 1);

        // Second diff should be empty — cache was updated.
        let second = mgr.diff();
        assert!(
            second.is_empty(),
            "second diff should be empty after cache update"
        );
    }

    #[test]
    fn diff_multiple_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let modify_path = dir.path().join("modify.rs");
        let delete_path = dir.path().join("delete.rs");
        std::fs::write(&modify_path, "fn m() {}\n").expect("write");
        set_mtime(&modify_path, 1_000_000);
        std::fs::write(&delete_path, "fn d() {}\n").expect("write");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        // Create, modify, and delete in one pass.
        std::fs::write(dir.path().join("create.rs"), "fn c() {}\n").expect("write");
        std::fs::write(&modify_path, "fn modified() {}\n").expect("write");
        set_mtime(&modify_path, 2_000_000);
        std::fs::remove_file(&delete_path).expect("remove");

        let changes = mgr.diff();
        assert!(
            changes
                .iter()
                .any(|c| c.path.ends_with("create.rs") && c.change_type == FileChangeType::Created),
            "missing Created, got: {changes:?}",
        );
        assert!(
            changes
                .iter()
                .any(|c| c.path.ends_with("modify.rs") && c.change_type == FileChangeType::Changed),
            "missing Changed, got: {changes:?}",
        );
        assert!(
            changes
                .iter()
                .any(|c| c.path.ends_with("delete.rs") && c.change_type == FileChangeType::Deleted),
            "missing Deleted, got: {changes:?}",
        );
    }

    #[test]
    fn mark_current_refreshes_mtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("edited.rs");
        std::fs::write(&path, "fn e() {}\n").expect("write");
        set_mtime(&path, 1_000_000);

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        // Simulate an edit — change content and mtime.
        std::fs::write(&path, "fn edited() {}\n").expect("write");
        set_mtime(&path, 2_000_000);

        // mark_current refreshes the cache entry.
        mgr.mark_current(std::slice::from_ref(&path));

        // diff should see no changes for this file.
        let changes = mgr.diff();
        assert!(
            !changes.iter().any(|c| c.path == path),
            "mark_current should have prevented diff from reporting this file",
        );
    }

    #[test]
    fn mark_current_removes_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("doomed.rs");
        std::fs::write(&path, "fn d() {}\n").expect("write");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        std::fs::remove_file(&path).expect("remove");
        mgr.mark_current(std::slice::from_ref(&path));

        // File should be gone from cache — diff should not report it.
        let changes = mgr.diff();
        assert!(
            !changes.iter().any(|c| c.path == path),
            "mark_current should have removed deleted file from cache",
        );
    }

    #[test]
    fn seed_respects_gitignore() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Initialize a git repo so .gitignore is respected.
        std::fs::create_dir(dir.path().join(".git")).expect("mkdir .git");
        std::fs::write(dir.path().join(".gitignore"), "ignored/\n").expect("write gitignore");
        std::fs::create_dir(dir.path().join("ignored")).expect("mkdir ignored");
        std::fs::write(dir.path().join("ignored/secret.rs"), "fn s() {}\n").expect("write");
        std::fs::write(dir.path().join("visible.rs"), "fn v() {}\n").expect("write");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        let cache = mgr.cache.lock().expect("lock");
        let has_secret = cache
            .keys()
            .any(|p| p.to_string_lossy().contains("secret.rs"));
        let has_visible = cache
            .keys()
            .any(|p| p.to_string_lossy().contains("visible.rs"));
        drop(cache);
        assert!(!has_secret, "gitignored file should not be in cache");
        assert!(has_visible, "visible file should be in cache");
    }

    #[test]
    fn diff_respects_gitignore() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(".git")).expect("mkdir .git");
        std::fs::write(dir.path().join(".gitignore"), "ignored/\n").expect("write gitignore");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        // Create files in both ignored and visible locations.
        std::fs::create_dir(dir.path().join("ignored")).expect("mkdir ignored");
        std::fs::write(dir.path().join("ignored/new.rs"), "fn n() {}\n").expect("write");
        std::fs::write(dir.path().join("visible.rs"), "fn v() {}\n").expect("write");

        let changes = mgr.diff();
        assert!(
            !changes
                .iter()
                .any(|c| c.path.to_string_lossy().contains("ignored")),
            "gitignored file should not appear in diff, got: {changes:?}",
        );
        assert!(
            changes.iter().any(|c| c.path.ends_with("visible.rs")),
            "visible file should appear in diff, got: {changes:?}",
        );
    }

    #[test]
    fn add_root_then_diff_reports_new_files() {
        let dir_a = tempfile::tempdir().expect("tempdir a");
        let dir_b = tempfile::tempdir().expect("tempdir b");
        std::fs::write(dir_a.path().join("a.rs"), "fn a() {}\n").expect("write");
        std::fs::write(dir_b.path().join("b.rs"), "fn b() {}\n").expect("write");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir_a.path().to_path_buf()]);
        mgr.seed();

        // Add a second root.
        mgr.set_roots(vec![dir_a.path().to_path_buf(), dir_b.path().to_path_buf()]);

        let changes = mgr.diff();
        assert!(
            changes
                .iter()
                .any(|c| c.path.ends_with("b.rs") && c.change_type == FileChangeType::Created),
            "new root's files should appear as Created, got: {changes:?}",
        );
        // Existing root should have no changes.
        assert!(
            !changes.iter().any(|c| c.path.ends_with("a.rs")),
            "existing root files should not appear, got: {changes:?}",
        );
    }
}

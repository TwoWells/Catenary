// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
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
            FileKind::Binary => None,
        }
    }
}

/// File classification: binary or text.
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
struct CachedEntry {
    mtime: u64,
    kind: FileKind,
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

        // Check cache
        if let Ok(cache) = self.cache.lock()
            && let Some(entry) = cache.get(path)
            && entry.mtime == mtime
        {
            return FileInfo {
                mtime,
                size,
                root,
                kind: entry.kind,
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
            cache.insert(path.to_path_buf(), CachedEntry { mtime, kind });
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

    /// Returns the line count if the file is text, or `None` if binary.
    pub fn line_count(&self, path: &Path, metadata: &std::fs::Metadata) -> Option<usize> {
        match self.classify(path, metadata).kind {
            FileKind::Binary => None,
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
    if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
        let lang = match file_name {
            "Dockerfile" => "dockerfile",
            "Makefile" => "makefile",
            "CMakeLists.txt" => "cmake",
            "Cargo.toml" | "Cargo.lock" => "toml",
            _ => "",
        };
        if !lang.is_empty() {
            return Some(lang);
        }
    }

    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some("rust"),
        Some("go") => Some("go"),
        Some("py") => Some("python"),
        Some("js") => Some("javascript"),
        Some("ts") => Some("typescript"),
        Some("tsx") => Some("typescriptreact"),
        Some("jsx") => Some("javascriptreact"),
        Some("c") => Some("c"),
        Some("cpp" | "cc" | "cxx" | "h" | "hpp") => Some("cpp"),
        Some("cs") => Some("csharp"),
        Some("java") => Some("java"),
        Some("kt" | "kts") => Some("kotlin"),
        Some("swift") => Some("swift"),
        Some("rb") => Some("ruby"),
        Some("php") => Some("php"),
        Some("sh" | "bash" | "zsh") => Some("shellscript"),
        Some("json") => Some("json"),
        Some("yaml" | "yml") => Some("yaml"),
        Some("toml") => Some("toml"),
        Some("md") => Some("markdown"),
        Some("html") => Some("html"),
        Some("css") => Some("css"),
        Some("scss") => Some("scss"),
        Some("lua") => Some("lua"),
        Some("sql") => Some("sql"),
        Some("zig") => Some("zig"),
        Some("mojo") => Some("mojo"),
        Some("dart") => Some("dart"),
        Some("m" | "mm") => Some("objective-c"),
        Some("nix") => Some("nix"),
        Some("proto") => Some("proto"),
        Some("graphql" | "gql") => Some("graphql"),
        Some("r" | "R") => Some("r"),
        Some("jl") => Some("julia"),
        Some("scala" | "sc") => Some("scala"),
        Some("hs") => Some("haskell"),
        Some("ex" | "exs") => Some("elixir"),
        Some("erl" | "hrl") => Some("erlang"),
        Some("cmake") => Some("cmake"),
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
        "python" | "python3" | "python2" => Some("python"),
        "node" | "nodejs" => Some("javascript"),
        "ruby" => Some("ruby"),
        "perl" => Some("perl"),
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
    fn language_detection_extensions() {
        assert_eq!(detect_language_id_opt(Path::new("test.rs")), Some("rust"));
        assert_eq!(detect_language_id_opt(Path::new("test.py")), Some("python"));
        assert_eq!(
            detect_language_id_opt(Path::new("test.js")),
            Some("javascript")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.ts")),
            Some("typescript")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.tsx")),
            Some("typescriptreact")
        );
        assert_eq!(detect_language_id_opt(Path::new("test.go")), Some("go"));
        assert_eq!(detect_language_id_opt(Path::new("test.php")), Some("php"));
        assert_eq!(
            detect_language_id_opt(Path::new("test.sh")),
            Some("shellscript")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.bash")),
            Some("shellscript")
        );
        assert_eq!(detect_language_id_opt(Path::new("test.cs")), Some("csharp"));
        assert_eq!(detect_language_id_opt(Path::new("test.kt")), Some("kotlin"));
        assert_eq!(
            detect_language_id_opt(Path::new("test.swift")),
            Some("swift")
        );
        assert_eq!(detect_language_id_opt(Path::new("test.html")), Some("html"));
        assert_eq!(detect_language_id_opt(Path::new("test.css")), Some("css"));
        assert_eq!(detect_language_id_opt(Path::new("test.scss")), Some("scss"));
        assert_eq!(
            detect_language_id_opt(Path::new("Dockerfile")),
            Some("dockerfile")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("Makefile")),
            Some("makefile")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("CMakeLists.txt")),
            Some("cmake")
        );
        assert_eq!(detect_language_id_opt(Path::new("test.zig")), Some("zig"));
        assert_eq!(detect_language_id_opt(Path::new("test.nix")), Some("nix"));
        assert_eq!(
            detect_language_id_opt(Path::new("test.proto")),
            Some("proto")
        );
        assert_eq!(
            detect_language_id_opt(Path::new("test.graphql")),
            Some("graphql")
        );
        assert_eq!(detect_language_id_opt(Path::new("test.r")), Some("r"));
        assert_eq!(detect_language_id_opt(Path::new("test.jl")), Some("julia"));
        assert_eq!(detect_language_id_opt(Path::new("test.ex")), Some("elixir"));
        assert_eq!(
            detect_language_id_opt(Path::new("Cargo.toml")),
            Some("toml")
        );
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
}

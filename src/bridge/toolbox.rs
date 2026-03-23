// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Shared container for tool servers and cross-tool infrastructure.
//!
//! Owns the tool implementations and the dependencies they share.
//! `LspBridgeHandler` holds a `Toolbox` and handles protocol boundary
//! concerns (health checks, readiness, dispatch routing).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::Mutex;

use super::DocumentManager;
use super::diagnostics_server::DiagnosticsServer;
use super::editing::EditingServer;
use super::file_tools::GlobServer;
use super::grep_server::GrepServer;
use super::replace::ReplaceServer;
use crate::lsp::ClientManager;

/// Files above this size are assumed binary without reading.
const BINARY_SIZE_THRESHOLD: u64 = 10 * 1024 * 1024; // 10 MB

/// Cross-tool filesystem metadata cache.
///
/// Caches binary detection results keyed by `(path, mtime_secs)` to avoid
/// redundant file reads on network filesystems (sshfs, NFS). Shared by
/// `GrepServer` and `GlobServer` through `Toolbox`.
pub struct FilesystemCache {
    binary: std::sync::Mutex<HashMap<PathBuf, (u64, bool)>>,
}

impl Default for FilesystemCache {
    fn default() -> Self {
        Self {
            binary: std::sync::Mutex::new(HashMap::new()),
        }
    }
}

impl FilesystemCache {
    /// Creates an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if the file is binary, using the cache when possible.
    ///
    /// Cache is keyed by absolute path + mtime. On mtime change the entry is
    /// re-scanned. Paths must be absolute before calling.
    pub fn is_binary(&self, path: &Path, metadata: &std::fs::Metadata) -> bool {
        let mtime = mtime_secs(metadata);

        // Check cache
        if let Ok(cache) = self.binary.lock()
            && let Some(&(cached_mtime, is_bin)) = cache.get(path)
            && cached_mtime == mtime
        {
            return is_bin;
        }

        let result = scan_file(path, metadata).is_none();

        // Update cache
        if let Ok(mut cache) = self.binary.lock() {
            cache.insert(path.to_path_buf(), (mtime, result));
        }

        result
    }

    /// Returns the line count if the file is text, or `None` if binary.
    ///
    /// Reads the file at most once — binary detection and line counting
    /// happen in a single pass. Uses the cache for the binary check;
    /// on cache hit for a text file, falls through to `count_lines`
    /// (one read). On cache miss, `scan_file` reads once and counts
    /// lines simultaneously.
    pub fn line_count(&self, path: &Path, metadata: &std::fs::Metadata) -> Option<usize> {
        let mtime = mtime_secs(metadata);

        // Check cache for binary status
        if let Ok(cache) = self.binary.lock()
            && let Some(&(cached_mtime, is_bin)) = cache.get(path)
            && cached_mtime == mtime
        {
            if is_bin {
                return None;
            }
            // Known text file — still need line count (not cached)
            return Some(count_lines(path));
        }

        // Cache miss: scan once for both binary detection and line count
        let line_count = scan_file(path, metadata);

        // Update binary cache
        if let Ok(mut cache) = self.binary.lock() {
            cache.insert(path.to_path_buf(), (mtime, line_count.is_none()));
        }

        line_count
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

/// Counts lines in a file (separate read, used on cache hit for text files).
fn count_lines(path: &Path) -> usize {
    let Ok(file) = std::fs::File::open(path) else {
        return 0;
    };
    let mut reader = std::io::BufReader::new(file);
    let mut count = 0;
    let mut buf = [0u8; 8192];
    loop {
        let Ok(n) = reader.read(&mut buf) else {
            return count;
        };
        if n == 0 {
            return count;
        }
        count += memchr::memchr_iter(b'\n', &buf[..n]).count();
    }
}

/// Scans a file for null bytes and counts lines in one pass.
///
/// Returns `Some(line_count)` for text files, `None` for binary files.
/// Files above the size threshold are assumed binary without reading.
fn scan_file(path: &Path, metadata: &std::fs::Metadata) -> Option<usize> {
    if metadata.len() > BINARY_SIZE_THRESHOLD {
        return None;
    }

    let Ok(file) = std::fs::File::open(path) else {
        return Some(0); // Can't open → treat as empty text
    };

    let mut reader = std::io::BufReader::new(file);
    let mut buf = [0u8; 8192];
    let mut lines = 0;
    loop {
        let Ok(n) = reader.read(&mut buf) else {
            return Some(lines);
        };
        if n == 0 {
            return Some(lines); // EOF, no nulls → text
        }
        if memchr::memchr(0, &buf[..n]).is_some() {
            return None; // Binary
        }
        lines += memchr::memchr_iter(b'\n', &buf[..n]).count();
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

/// Shared container for tool servers and cross-tool infrastructure.
///
/// Owns the tool implementations and the dependencies they share.
/// [`super::handler::LspBridgeHandler`] holds a `Toolbox` and handles protocol boundary
/// concerns (health checks, readiness, dispatch routing).
pub struct Toolbox {
    /// Grep tool server.
    pub grep: GrepServer,
    /// Glob tool server.
    pub glob: GlobServer,
    /// Batch replacement tool with snapshots and diagnostics.
    pub replace: ReplaceServer,
    /// Per-file diagnostic batching (`start_editing` / `done_editing`).
    pub editing: EditingServer,
    /// Shared LSP client manager.
    pub client_manager: Arc<ClientManager>,
    /// Shared document manager.
    pub doc_manager: Arc<Mutex<DocumentManager>>,
    /// Tokio runtime handle for blocking dispatch.
    pub runtime: Handle,
    /// Cross-tool filesystem metadata cache (binary detection).
    pub fs_cache: Arc<FilesystemCache>,
}

impl Toolbox {
    /// Creates a new `Toolbox` with all tool servers and shared dependencies.
    pub fn new(
        client_manager: Arc<ClientManager>,
        doc_manager: Arc<Mutex<DocumentManager>>,
        runtime: Handle,
        diagnostics: Arc<DiagnosticsServer>,
        session_id: Option<String>,
    ) -> Self {
        let fs_cache = Arc::new(FilesystemCache::new());
        let editing =
            EditingServer::new(diagnostics.clone(), session_id.clone().unwrap_or_default());
        let replace = ReplaceServer::new(
            client_manager.clone(),
            doc_manager.clone(),
            diagnostics,
            runtime.clone(),
            session_id,
        );
        let grep = GrepServer {
            client_manager: client_manager.clone(),
            doc_manager: doc_manager.clone(),
            runtime: runtime.clone(),
        };
        let glob = GlobServer {
            client_manager: client_manager.clone(),
            doc_manager: doc_manager.clone(),
            runtime: runtime.clone(),
        };
        Self {
            grep,
            glob,
            replace,
            editing,
            client_manager,
            doc_manager,
            runtime,
            fs_cache,
        }
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

    #[test]
    fn test_scan_binary_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("binary.bin");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(&[0x89, 0x50, 0x4E, 0x47, 0x00, 0x0A])
            .expect("write");
        drop(f);

        let metadata = std::fs::metadata(&path).expect("metadata");
        assert!(scan_file(&path, &metadata).is_none());
    }

    #[test]
    fn test_scan_text_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("text.txt");
        std::fs::write(&path, "Hello, world!\nLine two.\n").expect("write");

        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(scan_file(&path, &metadata), Some(2));
    }

    #[test]
    fn test_scan_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").expect("write");

        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(scan_file(&path, &metadata), Some(0));
    }

    #[test]
    fn test_line_count_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("code.rs");
        std::fs::write(&path, "fn main() {\n    println!(\"hi\");\n}\n").expect("write");

        let cache = FilesystemCache::new();
        let metadata = std::fs::metadata(&path).expect("metadata");

        // First call: scan + count in one pass
        assert_eq!(cache.line_count(&path, &metadata), Some(3));
        // Second call: cache hit for binary status, separate count_lines
        assert_eq!(cache.line_count(&path, &metadata), Some(3));
    }

    #[test]
    fn test_line_count_binary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("image.png");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(&[0x89, 0x50, 0x4E, 0x47, 0x00]).expect("write");
        drop(f);

        let cache = FilesystemCache::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(cache.line_count(&path, &metadata), None);
    }

    #[test]
    fn test_cache_populated_by_scan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cached.bin");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(&[0x00, 0x01, 0x02]).expect("write");
        drop(f);

        let cache = FilesystemCache::new();
        let metadata = std::fs::metadata(&path).expect("metadata");

        // First call: populates cache
        assert!(cache.is_binary(&path, &metadata));
        // Second call: uses cache (same mtime)
        assert!(cache.is_binary(&path, &metadata));

        let len = cache.binary.lock().expect("lock").len();
        assert_eq!(len, 1);
    }

    #[test]
    fn test_format_file_size() {
        assert_eq!(format_file_size(0), "0 B");
        assert_eq!(format_file_size(512), "512 B");
        assert_eq!(format_file_size(1024), "1 KB");
        assert_eq!(format_file_size(1_048_576), "1.0 MB");
        assert_eq!(format_file_size(1_073_741_824), "1.0 GB");
        assert_eq!(format_file_size(5_368_709_120), "5.0 GB");
    }
}

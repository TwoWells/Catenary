// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

use std::collections::HashMap;
use std::path::Path;

use crate::lsp::lang::path_to_uri;

/// Document version tracker and URI generator.
///
/// Tracks the document version per URI. The version starts at 1
/// on first open and increments on each subsequent content sync
/// (`didChange`). This monotonically increasing counter is required by
/// the LSP protocol and used by the diagnostics wait logic to match
/// pushed diagnostics to a specific document change.
///
/// Per-client open/close state is tracked by `LspClient::open_documents`.
/// This manager only provides versions and URI generation.
pub struct DocumentManager {
    /// Document versions, keyed by URI.
    documents: HashMap<String, i32>,
}

impl Default for DocumentManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DocumentManager {
    /// Creates a new, empty `DocumentManager`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            documents: HashMap::new(),
        }
    }

    /// Registers an open for a URI and returns the version.
    ///
    /// First open returns 1. Subsequent opens increment the version.
    pub fn open(&mut self, uri: &str) -> i32 {
        let version = self.documents.entry(uri.to_string()).or_insert(0);
        *version += 1;
        *version
    }

    /// Removes version tracking for a URI.
    pub fn close(&mut self, uri: &str) {
        self.documents.remove(uri);
    }

    /// Returns the `file://` URI for a path after canonicalization.
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be canonicalized.
    pub fn uri_for_path(&self, path: &Path) -> anyhow::Result<String> {
        Ok(path_to_uri(&path.canonicalize()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_returns_incrementing_versions() {
        let mut dm = DocumentManager::new();
        assert_eq!(dm.open("file:///test.rs"), 1);
        assert_eq!(dm.open("file:///test.rs"), 2);
        assert_eq!(dm.open("file:///test.rs"), 3);
    }

    #[test]
    fn close_removes_entry() {
        let mut dm = DocumentManager::new();
        assert_eq!(dm.open("file:///test.rs"), 1);
        assert_eq!(dm.open("file:///test.rs"), 2);
        dm.close("file:///test.rs");
        // Re-open starts fresh at 1.
        assert_eq!(dm.open("file:///test.rs"), 1);
    }

    #[test]
    fn close_unknown_uri_is_noop() {
        let mut dm = DocumentManager::new();
        dm.close("file:///unknown.rs"); // should not panic
    }

    #[test]
    fn independent_uris() {
        let mut dm = DocumentManager::new();
        assert_eq!(dm.open("file:///a.rs"), 1);
        assert_eq!(dm.open("file:///b.rs"), 1);
        assert_eq!(dm.open("file:///a.rs"), 2);
        assert_eq!(dm.open("file:///b.rs"), 2);
    }
}

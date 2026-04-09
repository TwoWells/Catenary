// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

use std::collections::HashMap;
use std::path::Path;

use crate::lsp::lang::path_to_uri;

/// Ref-counted document lifecycle owner.
///
/// Tracks which documents are open by URI and serializes
/// `didOpen`/`didClose` across concurrent consumers. Multiple agents
/// may have overlapping files open — this manager tracks ref counts
/// per URI, sends `didOpen` on first open and `didClose` on last close.
pub struct DocumentManager {
    /// Open document ref counts, keyed by URI.
    open_counts: HashMap<String, usize>,
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
            open_counts: HashMap::new(),
        }
    }

    /// Registers an open for a URI.
    ///
    /// Returns `true` if this is the first open (caller should send
    /// `didOpen`). Returns `false` if already open (no protocol message
    /// needed — just a ref-count bump).
    pub fn open(&mut self, uri: &str) -> bool {
        let count = self.open_counts.entry(uri.to_string()).or_insert(0);
        *count += 1;
        *count == 1
    }

    /// Registers a close for a URI.
    ///
    /// Returns `true` if the ref count reached zero (caller should send
    /// `didClose`). Returns `false` if other consumers still hold it
    /// open, or if the URI was not open.
    pub fn close(&mut self, uri: &str) -> bool {
        let Some(count) = self.open_counts.get_mut(uri) else {
            return false;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.open_counts.remove(uri);
            true
        } else {
            false
        }
    }

    /// Returns whether a URI is currently open (ref count > 0).
    #[must_use]
    pub fn is_open(&self, uri: &str) -> bool {
        self.open_counts.get(uri).is_some_and(|&c| c > 0)
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
    fn first_open_returns_true() {
        let mut dm = DocumentManager::new();
        assert!(dm.open("file:///test.rs"));
    }

    #[test]
    fn second_open_returns_false() {
        let mut dm = DocumentManager::new();
        assert!(dm.open("file:///test.rs"));
        assert!(!dm.open("file:///test.rs"));
    }

    #[test]
    fn close_with_one_ref_returns_true() {
        let mut dm = DocumentManager::new();
        dm.open("file:///test.rs");
        assert!(dm.close("file:///test.rs"));
    }

    #[test]
    fn close_with_multiple_refs_returns_false() {
        let mut dm = DocumentManager::new();
        dm.open("file:///test.rs");
        dm.open("file:///test.rs");
        assert!(!dm.close("file:///test.rs"));
        assert!(dm.close("file:///test.rs"));
    }

    #[test]
    fn close_unknown_uri_returns_false() {
        let mut dm = DocumentManager::new();
        assert!(!dm.close("file:///unknown.rs"));
    }

    #[test]
    fn double_close_returns_false() {
        let mut dm = DocumentManager::new();
        dm.open("file:///test.rs");
        assert!(dm.close("file:///test.rs"));
        assert!(!dm.close("file:///test.rs"));
    }

    #[test]
    fn is_open_tracks_state() {
        let mut dm = DocumentManager::new();
        assert!(!dm.is_open("file:///test.rs"));
        dm.open("file:///test.rs");
        assert!(dm.is_open("file:///test.rs"));
        dm.close("file:///test.rs");
        assert!(!dm.is_open("file:///test.rs"));
    }

    #[test]
    fn interleaved_consumers() {
        let mut dm = DocumentManager::new();
        let uri = "file:///shared.rs";

        // Agent A opens
        assert!(dm.open(uri));
        assert!(dm.is_open(uri));

        // Agent B opens (already open)
        assert!(!dm.open(uri));

        // Agent A closes (B still holds)
        assert!(!dm.close(uri));
        assert!(dm.is_open(uri));

        // Agent B closes (last ref)
        assert!(dm.close(uri));
        assert!(!dm.is_open(uri));
    }
}

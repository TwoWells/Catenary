// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

use std::collections::HashMap;
use std::path::Path;

use crate::lsp::lang::path_to_uri;

/// Per-URI open state: ref count and document version.
struct OpenState {
    refs: usize,
    version: i32,
}

/// Ref-counted document lifecycle owner.
///
/// Tracks which documents are open by URI and serializes
/// `didOpen`/`didClose` across concurrent consumers. Multiple agents
/// may have overlapping files open — this manager tracks ref counts
/// per URI, sends `didOpen` on first open and `didClose` on last close.
///
/// Also tracks the document version per URI. The version starts at 1
/// on first open and increments on each subsequent content sync
/// (`didChange`). This monotonically increasing counter is required by
/// the LSP protocol and used by the diagnostics wait logic to match
/// pushed diagnostics to a specific document change.
pub struct DocumentManager {
    /// Open document state, keyed by URI.
    documents: HashMap<String, OpenState>,
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

    /// Registers an open for a URI.
    ///
    /// Returns `(is_first, version)`. When `is_first` is `true`, the
    /// caller should send `didOpen` with the returned version (always 1).
    /// When `false`, the document is already open — the caller should
    /// send `didChange` with the returned (incremented) version.
    pub fn open(&mut self, uri: &str) -> (bool, i32) {
        let state = self.documents.entry(uri.to_string()).or_insert(OpenState {
            refs: 0,
            version: 0,
        });
        state.refs += 1;
        if state.refs == 1 {
            state.version = 1;
            (true, 1)
        } else {
            state.version += 1;
            (false, state.version)
        }
    }

    /// Registers a close for a URI.
    ///
    /// Returns `true` if the ref count reached zero (caller should send
    /// `didClose`). Returns `false` if other consumers still hold it
    /// open, or if the URI was not open.
    pub fn close(&mut self, uri: &str) -> bool {
        let Some(state) = self.documents.get_mut(uri) else {
            return false;
        };
        state.refs = state.refs.saturating_sub(1);
        if state.refs == 0 {
            self.documents.remove(uri);
            true
        } else {
            false
        }
    }

    /// Returns whether a URI is currently open (ref count > 0).
    #[must_use]
    pub fn is_open(&self, uri: &str) -> bool {
        self.documents.get(uri).is_some_and(|s| s.refs > 0)
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
    fn first_open_returns_true_version_1() {
        let mut dm = DocumentManager::new();
        assert_eq!(dm.open("file:///test.rs"), (true, 1));
    }

    #[test]
    fn second_open_returns_false_with_incremented_version() {
        let mut dm = DocumentManager::new();
        assert_eq!(dm.open("file:///test.rs"), (true, 1));
        assert_eq!(dm.open("file:///test.rs"), (false, 2));
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
    fn reopen_after_close_resets_version() {
        let mut dm = DocumentManager::new();
        assert_eq!(dm.open("file:///test.rs"), (true, 1));
        dm.close("file:///test.rs");
        assert_eq!(dm.open("file:///test.rs"), (true, 1));
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
        assert_eq!(dm.open(uri), (true, 1));
        assert!(dm.is_open(uri));

        // Agent B opens (already open, version bumps)
        assert_eq!(dm.open(uri), (false, 2));

        // Agent A closes (B still holds)
        assert!(!dm.close(uri));
        assert!(dm.is_open(uri));

        // Agent B closes (last ref)
        assert!(dm.close(uri));
        assert!(!dm.is_open(uri));
    }
}

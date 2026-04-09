// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Standalone pure functions for LSP document identity.
//!
//! These are stateless utilities extracted from `DocumentManager` so
//! callers don't need to acquire a lock for simple conversions.

use std::path::Path;

/// Converts a filesystem path to a `file://` URI.
///
/// The path should be absolute and canonical for correct results.
/// No percent-encoding is applied — LSP servers accept literal paths
/// on Linux/macOS.
#[must_use]
pub fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_path() {
        assert_eq!(
            path_to_uri(Path::new("/home/user/test.rs")),
            "file:///home/user/test.rs"
        );
    }

    #[test]
    fn root_path() {
        assert_eq!(path_to_uri(Path::new("/")), "file:///");
    }

    #[test]
    fn nested_path() {
        assert_eq!(path_to_uri(Path::new("/a/b/c/d.py")), "file:///a/b/c/d.py");
    }
}

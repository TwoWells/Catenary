// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Small local types for LSP concepts.
//!
//! URIs are plain `String` — no `url` crate. These types are internal
//! and converted to/from `serde_json::Value` via builders and extractors.

/// A position in a text document (0-indexed line and character).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Zero-based line number.
    pub line: u32,
    /// Zero-based character offset (encoding determined by negotiation).
    pub character: u32,
}

/// A range in a text document (exclusive end).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    /// The range's start position (inclusive).
    pub start: Position,
    /// The range's end position (exclusive).
    pub end: Position,
}

/// A workspace folder with a URI and display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceFolder {
    /// The URI for this workspace folder.
    pub uri: String,
    /// The human-readable name for this workspace folder.
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::{Position, Range, WorkspaceFolder};

    #[test]
    fn position_equality() {
        let a = Position {
            line: 1,
            character: 2,
        };
        let b = Position {
            line: 1,
            character: 2,
        };
        assert_eq!(a, b);
    }

    #[test]
    fn position_inequality() {
        let a = Position {
            line: 1,
            character: 2,
        };
        let b = Position {
            line: 1,
            character: 3,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn position_copy() {
        let a = Position {
            line: 1,
            character: 2,
        };
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn range_equality() {
        let a = Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 10,
                character: 5,
            },
        };
        let b = Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 10,
                character: 5,
            },
        };
        assert_eq!(a, b);
    }

    #[test]
    fn range_copy() {
        let a = Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 1,
                character: 1,
            },
        };
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn workspace_folder_equality() {
        let a = WorkspaceFolder {
            uri: "file:///foo".to_string(),
            name: "foo".to_string(),
        };
        let b = WorkspaceFolder {
            uri: "file:///foo".to_string(),
            name: "foo".to_string(),
        };
        assert_eq!(a, b);
    }

    #[test]
    fn workspace_folder_clone() {
        let a = WorkspaceFolder {
            uri: "file:///bar".to_string(),
            name: "bar".to_string(),
        };
        let b = a.clone();
        assert_eq!(a, b);
    }
}

// SPDX-License-Identifier: AGPL-3.0-or-later
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

#[cfg(test)]
mod tests {
    use super::{Position, Range};

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
}

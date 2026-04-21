// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Shared symbol types and helpers for handler and file_tools.

// Symbol kind constants (LSP spec values)
pub(super) const SK_MODULE: u32 = 2;
pub(super) const SK_NAMESPACE: u32 = 3;
pub(super) const SK_PACKAGE: u32 = 4;
pub(super) const SK_CLASS: u32 = 5;
pub(super) const SK_METHOD: u32 = 6;
pub(super) const SK_CONSTRUCTOR: u32 = 9;
pub(super) const SK_ENUM: u32 = 10;
pub(super) const SK_INTERFACE: u32 = 11;
pub(super) const SK_FUNCTION: u32 = 12;
pub(super) const SK_VARIABLE: u32 = 13;
pub(super) const SK_CONSTANT: u32 = 14;
pub(super) const SK_STRING: u32 = 15;
pub(super) const SK_OBJECT: u32 = 19;
pub(super) const SK_KEY: u32 = 20;
pub(super) const SK_STRUCT: u32 = 23;

/// Formats a symbol kind number as a human-readable string.
pub(super) const fn format_symbol_kind(kind: u32) -> &'static str {
    match kind {
        1 => "File",
        SK_MODULE => "Module",
        SK_NAMESPACE => "Namespace",
        SK_PACKAGE => "Package",
        SK_CLASS => "Class",
        SK_METHOD => "Method",
        7 => "Property",
        8 => "Field",
        SK_CONSTRUCTOR => "Constructor",
        SK_ENUM => "Enum",
        SK_INTERFACE => "Interface",
        SK_FUNCTION => "Function",
        SK_VARIABLE => "Variable",
        SK_CONSTANT => "Constant",
        SK_STRING => "String",
        16 => "Number",
        17 => "Boolean",
        18 => "Array",
        SK_OBJECT => "Object",
        SK_KEY => "Key",
        21 => "Null",
        22 => "EnumMember",
        SK_STRUCT => "Struct",
        24 => "Event",
        25 => "Operator",
        26 => "TypeParameter",
        _ => "Unknown",
    }
}

/// Returns `true` for symbol kinds included in outline output.
pub(super) const fn is_outline_kind(kind: u32) -> bool {
    matches!(
        kind,
        SK_STRUCT
            | SK_CLASS
            | SK_ENUM
            | SK_INTERFACE
            | SK_MODULE
            | SK_NAMESPACE
            | SK_PACKAGE
            | SK_CONSTANT
            | SK_OBJECT
            | SK_STRING
            | SK_KEY
    )
}

// `extract_symbol_infos` and `extract_locations` deleted by SEARCHv2 ticket 06a.
// Tree-sitter index replaces workspace/symbol as the symbol source.

// `extract_locations`, `symbol_from_location`, `range_start_line`,
// `range_start_character`, `uri_to_path` deleted by SEARCHv2 ticket 06a.

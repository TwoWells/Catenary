// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Shared symbol types and helpers for handler and file_tools.

use serde_json::Value;

/// Lightweight symbol info extracted from LSP workspace/symbol responses.
pub(super) struct SymbolInfo {
    /// Symbol name.
    pub name: String,
    /// Symbol kind (LSP numeric value).
    pub kind: u32,
    /// Absolute file path (decoded from URI).
    pub file_path: String,
    /// 0-based line number.
    pub line: u32,
    /// 0-based character offset.
    pub character: u32,
}

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

/// Extracts `SymbolInfo` values from a workspace/symbol response.
///
/// Handles both flat (`SymbolInformation[]`) and nested (`WorkspaceSymbol[]`)
/// response shapes. For nested symbols with `location.uri` only (no range),
/// returns `None` for the symbol (caller should resolve).
pub(super) fn extract_symbol_infos(response: &Value) -> Vec<SymbolInfo> {
    let Some(arr) = response.as_array() else {
        return Vec::new();
    };

    let mut result = Vec::new();
    for item in arr {
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        let kind = item
            .get("kind")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0);

        // Flat SymbolInformation: location.uri + location.range
        if let Some(location) = item.get("location")
            && let Some(info) = symbol_from_location(name, kind, location)
        {
            result.push(info);
            continue;
        }

        // Nested WorkspaceSymbol: location may be { uri } or { uri, range }
        // If range is present, extract it. Otherwise skip (needs resolve).
        if let Some(uri_str) = item
            .get("location")
            .and_then(|l| l.get("uri"))
            .and_then(Value::as_str)
            && let Some(range) = item.get("location").and_then(|l| l.get("range"))
        {
            let line = range_start_line(range);
            let character = range_start_character(range);
            if let Some(path) = uri_to_path(uri_str) {
                result.push(SymbolInfo {
                    name: name.to_string(),
                    kind,
                    file_path: path,
                    line,
                    character,
                });
            }
            // else: URI-only, caller must resolve
        }
    }

    result
}

/// Extracts `(file_path, line, character)` tuples from a goto-definition,
/// implementation, or references response.
///
/// Handles scalar `Location`, `Location[]`, and `LocationLink[]` shapes.
pub(super) fn extract_locations(response: &Value) -> Vec<(String, u32, u32)> {
    // Null response
    if response.is_null() {
        return Vec::new();
    }

    // Array of Location or LocationLink
    if let Some(arr) = response.as_array() {
        let mut result = Vec::new();
        for item in arr {
            // LocationLink: has targetUri and targetRange
            if let Some(uri) = item.get("targetUri").and_then(Value::as_str) {
                if let Some(path) = uri_to_path(uri) {
                    let line = item.get("targetRange").map_or(0, range_start_line);
                    let character = item.get("targetRange").map_or(0, range_start_character);
                    result.push((path, line, character));
                }
            }
            // Location: has uri and range
            else if let Some(uri) = item.get("uri").and_then(Value::as_str)
                && let Some(path) = uri_to_path(uri)
            {
                let line = item.get("range").map_or(0, range_start_line);
                let character = item.get("range").map_or(0, range_start_character);
                result.push((path, line, character));
            }
        }
        return result;
    }

    // Scalar Location
    if let Some(uri) = response.get("uri").and_then(Value::as_str)
        && let Some(path) = uri_to_path(uri)
    {
        let line = response.get("range").map_or(0, range_start_line);
        let character = response.get("range").map_or(0, range_start_character);
        return vec![(path, line, character)];
    }

    Vec::new()
}

/// Extracts a `SymbolInfo` from a `Location` object.
fn symbol_from_location(name: &str, kind: u32, location: &Value) -> Option<SymbolInfo> {
    let uri = location.get("uri")?.as_str()?;
    let path = uri_to_path(uri)?;
    let range = location.get("range")?;
    Some(SymbolInfo {
        name: name.to_string(),
        kind,
        file_path: path,
        line: range_start_line(range),
        character: range_start_character(range),
    })
}

/// Extracts `range.start.line` as `u32`.
fn range_start_line(range: &Value) -> u32 {
    range
        .get("start")
        .and_then(|s| s.get("line"))
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0)
}

/// Extracts `range.start.character` as `u32`.
fn range_start_character(range: &Value) -> u32 {
    range
        .get("start")
        .and_then(|s| s.get("character"))
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0)
}

/// Converts a `file://` URI to a filesystem path string.
pub(super) fn uri_to_path(uri: &str) -> Option<String> {
    uri.strip_prefix("file://").map(str::to_string)
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Builder functions for LSP request and notification parameters.
//!
//! One function per LSP method Catenary uses. Each constructs a
//! `serde_json::Value` from primitives — no `lsp_types` dependency.

use serde_json::{Value, json};

// ── Private helpers ─────────────────────────────────────────────────

/// Builds `TextDocumentPositionParams` — shared by hover, definition,
/// type definition, implementation, prepare rename, and hierarchy prepares.
fn text_document_position(uri: &str, line: u32, character: u32) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character }
    })
}

/// Converts `(uri, name)` pairs to a JSON array of workspace folders.
fn folder_array(folders: &[(&str, &str)]) -> Vec<Value> {
    folders
        .iter()
        .map(|(uri, name)| json!({ "uri": uri, "name": name }))
        .collect()
}

// ── Lifecycle ───────────────────────────────────────────────────────

/// Builds `InitializeParams` with the full `ClientCapabilities` that
/// Catenary advertises to servers.
///
/// `roots` is a slice of `(uri, name)` pairs for workspace folders.
#[must_use]
pub fn initialize(
    pid: u32,
    roots: &[(&str, &str)],
    initialization_options: Option<&Value>,
) -> Value {
    let workspace_folders = folder_array(roots);
    let root_uri = roots.first().map_or(Value::Null, |(uri, _)| json!(uri));

    let mut params = json!({
        "processId": pid,
        "rootUri": root_uri,
        "capabilities": {
            "general": {
                "positionEncodings": ["utf-8", "utf-16"]
            },
            "textDocument": {
                "synchronization": {
                    "didSave": true,
                    "dynamicRegistration": false,
                    "willSave": false,
                    "willSaveWaitUntil": false
                },
                "publishDiagnostics": {
                    "versionSupport": true
                },
                "definition": {
                    "dynamicRegistration": false,
                    "linkSupport": true
                },
                "typeDefinition": {
                    "dynamicRegistration": false,
                    "linkSupport": true
                },
                "implementation": {
                    "dynamicRegistration": false,
                    "linkSupport": true
                },
                "declaration": {
                    "dynamicRegistration": false,
                    "linkSupport": true
                },
                "references": {
                    "dynamicRegistration": false
                },
                "documentSymbol": {
                    "dynamicRegistration": false,
                    "hierarchicalDocumentSymbolSupport": true
                },
                "callHierarchy": {
                    "dynamicRegistration": false
                },
                "typeHierarchy": {
                    "dynamicRegistration": false
                },
                "codeAction": {
                    "dynamicRegistration": false
                }
            },
            "workspace": {
                "symbol": {
                    "resolveSupport": {
                        "properties": ["location.range"]
                    }
                },
                "workspaceFolders": true,
                "configuration": true
            },
            "window": {
                "workDoneProgress": true
            }
        },
        "workspaceFolders": workspace_folders
    });

    if let Some(opts) = initialization_options {
        params["initializationOptions"] = opts.clone();
    }

    params
}

// ── Document synchronization ────────────────────────────────────────

/// Builds `DidOpenTextDocumentParams`.
#[must_use]
pub fn did_open(uri: &str, language_id: &str, version: i32, text: &str) -> Value {
    json!({
        "textDocument": {
            "uri": uri,
            "languageId": language_id,
            "version": version,
            "text": text
        }
    })
}

/// Builds `DidChangeTextDocumentParams` with full content replacement.
#[must_use]
pub fn did_change(uri: &str, version: i32, text: &str) -> Value {
    json!({
        "textDocument": { "uri": uri, "version": version },
        "contentChanges": [{ "text": text }]
    })
}

/// Builds `DidCloseTextDocumentParams`.
#[must_use]
pub fn did_close(uri: &str) -> Value {
    json!({
        "textDocument": { "uri": uri }
    })
}

/// Builds `DidSaveTextDocumentParams` (without included text).
#[must_use]
pub fn did_save(uri: &str) -> Value {
    json!({
        "textDocument": { "uri": uri }
    })
}

// ── Workspace ───────────────────────────────────────────────────────

/// Builds `DidChangeWorkspaceFoldersParams`.
///
/// `added` and `removed` are slices of `(uri, name)` pairs.
#[must_use]
pub fn did_change_workspace_folders(added: &[(&str, &str)], removed: &[(&str, &str)]) -> Value {
    json!({
        "event": {
            "added": folder_array(added),
            "removed": folder_array(removed)
        }
    })
}

/// Builds `WorkspaceSymbolParams`.
#[must_use]
pub fn workspace_symbols(query: &str) -> Value {
    json!({ "query": query })
}

// ── Text document requests (position-based) ─────────────────────────

/// Builds `HoverParams`.
#[must_use]
pub fn hover(uri: &str, line: u32, character: u32) -> Value {
    text_document_position(uri, line, character)
}

/// Builds `DefinitionParams`.
#[must_use]
pub fn definition(uri: &str, line: u32, character: u32) -> Value {
    text_document_position(uri, line, character)
}

/// Builds `TypeDefinitionParams`.
#[must_use]
pub fn type_definition(uri: &str, line: u32, character: u32) -> Value {
    text_document_position(uri, line, character)
}

/// Builds `ImplementationParams`.
#[must_use]
pub fn implementation(uri: &str, line: u32, character: u32) -> Value {
    text_document_position(uri, line, character)
}

/// Builds `PrepareRenameParams`.
#[must_use]
pub fn prepare_rename(uri: &str, line: u32, character: u32) -> Value {
    text_document_position(uri, line, character)
}

/// Builds `ReferenceParams`.
#[must_use]
pub fn references(uri: &str, line: u32, character: u32, include_declaration: bool) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character },
        "context": { "includeDeclaration": include_declaration }
    })
}

/// Builds `DocumentSymbolParams`.
#[must_use]
pub fn document_symbols(uri: &str) -> Value {
    json!({ "textDocument": { "uri": uri } })
}

// ── Code actions ────────────────────────────────────────────────────

/// Builds `CodeActionParams`.
///
/// `diagnostics` are raw `Value` arrays (diagnostics stored as JSON).
#[must_use]
#[allow(
    dead_code,
    reason = "LSP primitives API — client inlines with 'only' filter"
)]
pub fn code_action(
    uri: &str,
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
    diagnostics: &[Value],
) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "range": {
            "start": { "line": start_line, "character": start_character },
            "end": { "line": end_line, "character": end_character }
        },
        "context": {
            "diagnostics": diagnostics
        }
    })
}

// ── Call hierarchy ──────────────────────────────────────────────────

/// Builds `CallHierarchyPrepareParams`.
#[must_use]
pub fn prepare_call_hierarchy(uri: &str, line: u32, character: u32) -> Value {
    text_document_position(uri, line, character)
}

/// Builds `CallHierarchyIncomingCallsParams`.
///
/// `item` is a pass-through `CallHierarchyItem` from the prepare response.
#[must_use]
pub fn incoming_calls(item: &Value) -> Value {
    json!({ "item": item })
}

/// Builds `CallHierarchyOutgoingCallsParams`.
///
/// `item` is a pass-through `CallHierarchyItem` from the prepare response.
#[must_use]
pub fn outgoing_calls(item: &Value) -> Value {
    json!({ "item": item })
}

// ── Type hierarchy ──────────────────────────────────────────────────

/// Builds `TypeHierarchyPrepareParams`.
#[must_use]
pub fn prepare_type_hierarchy(uri: &str, line: u32, character: u32) -> Value {
    text_document_position(uri, line, character)
}

/// Builds `TypeHierarchySupertypesParams`.
///
/// `item` is a pass-through `TypeHierarchyItem` from the prepare response.
#[must_use]
pub fn supertypes(item: &Value) -> Value {
    json!({ "item": item })
}

/// Builds `TypeHierarchySubtypesParams`.
///
/// `item` is a pass-through `TypeHierarchyItem` from the prepare response.
#[must_use]
pub fn subtypes(item: &Value) -> Value {
    json!({ "item": item })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    // ── Initialize ──────────────────────────────────────────────────

    #[test]
    fn initialize_single_root() {
        let ours = initialize(42, &[("file:///workspace", "workspace")], None);

        let expected = json!({
            "processId": 42,
            "rootUri": "file:///workspace",
            "capabilities": {
                "general": {
                    "positionEncodings": ["utf-8", "utf-16"]
                },
                "textDocument": {
                    "synchronization": {
                        "didSave": true,
                        "dynamicRegistration": false,
                        "willSave": false,
                        "willSaveWaitUntil": false
                    },
                    "publishDiagnostics": {
                        "versionSupport": true
                    },
                    "definition": {
                        "dynamicRegistration": false,
                        "linkSupport": true
                    },
                    "typeDefinition": {
                        "dynamicRegistration": false,
                        "linkSupport": true
                    },
                    "implementation": {
                        "dynamicRegistration": false,
                        "linkSupport": true
                    },
                    "declaration": {
                        "dynamicRegistration": false,
                        "linkSupport": true
                    },
                    "references": {
                        "dynamicRegistration": false
                    },
                    "documentSymbol": {
                        "dynamicRegistration": false,
                        "hierarchicalDocumentSymbolSupport": true
                    },
                    "callHierarchy": {
                        "dynamicRegistration": false
                    },
                    "typeHierarchy": {
                        "dynamicRegistration": false
                    },
                    "codeAction": {
                        "dynamicRegistration": false
                    }
                },
                "workspace": {
                    "symbol": {
                        "resolveSupport": {
                            "properties": ["location.range"]
                        }
                    },
                    "workspaceFolders": true,
                    "configuration": true
                },
                "window": {
                    "workDoneProgress": true
                }
            },
            "workspaceFolders": [
                { "uri": "file:///workspace", "name": "workspace" }
            ]
        });

        assert_eq!(ours, expected);
    }

    #[test]
    fn initialize_with_options() {
        let opts = json!({"key": "value"});
        let ours = initialize(1, &[("file:///ws", "ws")], Some(&opts));

        assert_eq!(ours["processId"], 1);
        assert_eq!(ours["rootUri"], "file:///ws");
        assert_eq!(ours["initializationOptions"], json!({"key": "value"}));
        assert_eq!(
            ours["workspaceFolders"],
            json!([{"uri": "file:///ws", "name": "ws"}])
        );
    }

    // ── Document synchronization ────────────────────────────────────

    #[test]
    fn did_open_golden() {
        let ours = did_open("file:///foo.rs", "rust", 1, "fn main() {}");

        assert_eq!(
            ours,
            json!({
                "textDocument": {
                    "uri": "file:///foo.rs",
                    "languageId": "rust",
                    "version": 1,
                    "text": "fn main() {}"
                }
            })
        );
    }

    #[test]
    fn did_change_golden() {
        let ours = did_change("file:///foo.rs", 2, "fn main() { println!() }");

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs", "version": 2 },
                "contentChanges": [{ "text": "fn main() { println!() }" }]
            })
        );
    }

    #[test]
    fn did_close_golden() {
        let ours = did_close("file:///foo.rs");

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" }
            })
        );
    }

    #[test]
    fn did_save_golden() {
        let ours = did_save("file:///foo.rs");

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" }
            })
        );
    }

    // ── Workspace ───────────────────────────────────────────────────

    #[test]
    fn did_change_workspace_folders_golden() {
        let ours =
            did_change_workspace_folders(&[("file:///new", "new")], &[("file:///old", "old")]);

        assert_eq!(
            ours,
            json!({
                "event": {
                    "added": [{ "uri": "file:///new", "name": "new" }],
                    "removed": [{ "uri": "file:///old", "name": "old" }]
                }
            })
        );
    }

    #[test]
    fn workspace_symbols_golden() {
        let ours = workspace_symbols("MyStruct");

        assert_eq!(ours, json!({ "query": "MyStruct" }));
    }

    // ── Position-based requests ─────────────────────────────────────

    #[test]
    fn hover_golden() {
        let ours = hover("file:///foo.rs", 10, 5);

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" },
                "position": { "line": 10, "character": 5 }
            })
        );
    }

    #[test]
    fn definition_golden() {
        let ours = definition("file:///foo.rs", 10, 5);

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" },
                "position": { "line": 10, "character": 5 }
            })
        );
    }

    #[test]
    fn type_definition_golden() {
        let ours = type_definition("file:///foo.rs", 3, 8);

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" },
                "position": { "line": 3, "character": 8 }
            })
        );
    }

    #[test]
    fn implementation_golden() {
        let ours = implementation("file:///foo.rs", 5, 12);

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" },
                "position": { "line": 5, "character": 12 }
            })
        );
    }

    #[test]
    fn prepare_rename_golden() {
        let ours = prepare_rename("file:///foo.rs", 7, 4);

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" },
                "position": { "line": 7, "character": 4 }
            })
        );
    }

    #[test]
    fn references_golden() {
        let ours = references("file:///foo.rs", 10, 5, true);

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" },
                "position": { "line": 10, "character": 5 },
                "context": { "includeDeclaration": true }
            })
        );
    }

    #[test]
    fn document_symbols_golden() {
        let ours = document_symbols("file:///foo.rs");

        assert_eq!(ours, json!({ "textDocument": { "uri": "file:///foo.rs" } }));
    }

    // ── Code actions ────────────────────────────────────────────────

    #[test]
    fn code_action_golden() {
        let ours = code_action("file:///foo.rs", 1, 0, 1, 10, &[]);

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" },
                "range": {
                    "start": { "line": 1, "character": 0 },
                    "end": { "line": 1, "character": 10 }
                },
                "context": {
                    "diagnostics": []
                }
            })
        );
    }

    // ── Call hierarchy ──────────────────────────────────────────────

    fn sample_call_hierarchy_item() -> Value {
        json!({
            "name": "foo",
            "kind": 12,
            "uri": "file:///foo.rs",
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 1, "character": 0 }
            },
            "selectionRange": {
                "start": { "line": 0, "character": 3 },
                "end": { "line": 0, "character": 6 }
            }
        })
    }

    #[test]
    fn prepare_call_hierarchy_golden() {
        let ours = prepare_call_hierarchy("file:///foo.rs", 5, 3);

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" },
                "position": { "line": 5, "character": 3 }
            })
        );
    }

    #[test]
    fn incoming_calls_golden() {
        let item = sample_call_hierarchy_item();
        let ours = incoming_calls(&item);

        assert_eq!(ours, json!({ "item": item }));
    }

    #[test]
    fn outgoing_calls_golden() {
        let item = sample_call_hierarchy_item();
        let ours = outgoing_calls(&item);

        assert_eq!(ours, json!({ "item": item }));
    }

    // ── Type hierarchy ──────────────────────────────────────────────

    fn sample_type_hierarchy_item() -> Value {
        json!({
            "name": "MyTrait",
            "kind": 11,
            "uri": "file:///foo.rs",
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 5, "character": 0 }
            },
            "selectionRange": {
                "start": { "line": 0, "character": 6 },
                "end": { "line": 0, "character": 13 }
            }
        })
    }

    #[test]
    fn prepare_type_hierarchy_golden() {
        let ours = prepare_type_hierarchy("file:///foo.rs", 2, 10);

        assert_eq!(
            ours,
            json!({
                "textDocument": { "uri": "file:///foo.rs" },
                "position": { "line": 2, "character": 10 }
            })
        );
    }

    #[test]
    fn supertypes_golden() {
        let item = sample_type_hierarchy_item();
        let ours = supertypes(&item);

        assert_eq!(ours, json!({ "item": item }));
    }

    #[test]
    fn subtypes_golden() {
        let item = sample_type_hierarchy_item();
        let ours = subtypes(&item);

        assert_eq!(ours, json!({ "item": item }));
    }
}

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
    clippy::default_trait_access,
    reason = "tests use expect for readable assertions and Default::default() for lsp_types construction"
)]
mod tests {
    use super::*;
    use lsp_types::{
        CallHierarchyIncomingCallsParams, CallHierarchyItem, CallHierarchyOutgoingCallsParams,
        CallHierarchyPrepareParams, ClientCapabilities, CodeActionContext, CodeActionParams,
        DidChangeTextDocumentParams, DidChangeWorkspaceFoldersParams, DidCloseTextDocumentParams,
        DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentSymbolParams,
        GotoDefinitionParams, HoverParams, InitializeParams, PositionEncodingKind,
        ReferenceContext, ReferenceParams, SymbolKind, TextDocumentContentChangeEvent,
        TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams,
        TypeHierarchyItem as LspTypeHierarchyItem,
        TypeHierarchyPrepareParams as LspTypeHierarchyPrepareParams,
        TypeHierarchySubtypesParams as LspTypeHierarchySubtypesParams,
        TypeHierarchySupertypesParams as LspTypeHierarchySupertypesParams,
        VersionedTextDocumentIdentifier, WorkspaceFolder as LspWorkspaceFolder,
        WorkspaceFoldersChangeEvent, WorkspaceSymbolParams,
    };

    // ── Initialize ──────────────────────────────────────────────────

    #[test]
    #[allow(deprecated, reason = "root_uri is deprecated in LSP but still tested")]
    fn initialize_matches_lsp_types() {
        let ours = initialize(42, &[("file:///workspace", "workspace")], None);

        let theirs = serde_json::to_value(InitializeParams {
            process_id: Some(42),
            capabilities: ClientCapabilities {
                general: Some(lsp_types::GeneralClientCapabilities {
                    position_encodings: Some(vec![
                        PositionEncodingKind::UTF8,
                        PositionEncodingKind::UTF16,
                    ]),
                    ..Default::default()
                }),
                text_document: Some(lsp_types::TextDocumentClientCapabilities {
                    synchronization: Some(lsp_types::TextDocumentSyncClientCapabilities {
                        did_save: Some(true),
                        dynamic_registration: Some(false),
                        will_save: Some(false),
                        will_save_wait_until: Some(false),
                    }),
                    publish_diagnostics: Some(lsp_types::PublishDiagnosticsClientCapabilities {
                        version_support: Some(true),
                        ..Default::default()
                    }),
                    definition: Some(lsp_types::GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(true),
                    }),
                    type_definition: Some(lsp_types::GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(true),
                    }),
                    implementation: Some(lsp_types::GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(true),
                    }),
                    declaration: Some(lsp_types::GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(true),
                    }),
                    references: Some(lsp_types::DynamicRegistrationClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    document_symbol: Some(lsp_types::DocumentSymbolClientCapabilities {
                        dynamic_registration: Some(false),
                        hierarchical_document_symbol_support: Some(true),
                        ..Default::default()
                    }),
                    call_hierarchy: Some(lsp_types::CallHierarchyClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    type_hierarchy: Some(lsp_types::TypeHierarchyClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    code_action: Some(lsp_types::CodeActionClientCapabilities {
                        dynamic_registration: Some(false),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                workspace: Some(lsp_types::WorkspaceClientCapabilities {
                    symbol: Some(lsp_types::WorkspaceSymbolClientCapabilities {
                        resolve_support: Some(lsp_types::WorkspaceSymbolResolveSupportCapability {
                            properties: vec!["location.range".to_string()],
                        }),
                        ..Default::default()
                    }),
                    workspace_folders: Some(true),
                    configuration: Some(true),
                    ..Default::default()
                }),
                window: Some(lsp_types::WindowClientCapabilities {
                    work_done_progress: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            },
            root_uri: Some("file:///workspace".parse().expect("valid uri")),
            workspace_folders: Some(vec![LspWorkspaceFolder {
                uri: "file:///workspace".parse().expect("valid uri"),
                name: "workspace".to_string(),
            }]),
            ..Default::default()
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    #[test]
    #[allow(deprecated, reason = "root_uri is deprecated in LSP but still tested")]
    fn initialize_with_options() {
        let opts = json!({"key": "value"});
        let ours = initialize(1, &[("file:///ws", "ws")], Some(&opts));

        let theirs = serde_json::to_value(InitializeParams {
            process_id: Some(1),
            capabilities: ClientCapabilities {
                general: Some(lsp_types::GeneralClientCapabilities {
                    position_encodings: Some(vec![
                        PositionEncodingKind::UTF8,
                        PositionEncodingKind::UTF16,
                    ]),
                    ..Default::default()
                }),
                text_document: Some(lsp_types::TextDocumentClientCapabilities {
                    synchronization: Some(lsp_types::TextDocumentSyncClientCapabilities {
                        did_save: Some(true),
                        dynamic_registration: Some(false),
                        will_save: Some(false),
                        will_save_wait_until: Some(false),
                    }),
                    publish_diagnostics: Some(lsp_types::PublishDiagnosticsClientCapabilities {
                        version_support: Some(true),
                        ..Default::default()
                    }),
                    definition: Some(lsp_types::GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(true),
                    }),
                    type_definition: Some(lsp_types::GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(true),
                    }),
                    implementation: Some(lsp_types::GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(true),
                    }),
                    declaration: Some(lsp_types::GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(true),
                    }),
                    references: Some(lsp_types::DynamicRegistrationClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    document_symbol: Some(lsp_types::DocumentSymbolClientCapabilities {
                        dynamic_registration: Some(false),
                        hierarchical_document_symbol_support: Some(true),
                        ..Default::default()
                    }),
                    call_hierarchy: Some(lsp_types::CallHierarchyClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    type_hierarchy: Some(lsp_types::TypeHierarchyClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    code_action: Some(lsp_types::CodeActionClientCapabilities {
                        dynamic_registration: Some(false),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                workspace: Some(lsp_types::WorkspaceClientCapabilities {
                    symbol: Some(lsp_types::WorkspaceSymbolClientCapabilities {
                        resolve_support: Some(lsp_types::WorkspaceSymbolResolveSupportCapability {
                            properties: vec!["location.range".to_string()],
                        }),
                        ..Default::default()
                    }),
                    workspace_folders: Some(true),
                    configuration: Some(true),
                    ..Default::default()
                }),
                window: Some(lsp_types::WindowClientCapabilities {
                    work_done_progress: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            },
            root_uri: Some("file:///ws".parse().expect("valid uri")),
            workspace_folders: Some(vec![LspWorkspaceFolder {
                uri: "file:///ws".parse().expect("valid uri"),
                name: "ws".to_string(),
            }]),
            initialization_options: Some(opts),
            ..Default::default()
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    // ── Document synchronization ────────────────────────────────────

    #[test]
    fn did_open_matches_lsp_types() {
        let ours = did_open("file:///foo.rs", "rust", 1, "fn main() {}");

        let theirs = serde_json::to_value(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: "file:///foo.rs".parse().expect("uri"),
                language_id: "rust".to_string(),
                version: 1,
                text: "fn main() {}".to_string(),
            },
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    #[test]
    fn did_change_matches_lsp_types() {
        let ours = did_change("file:///foo.rs", 2, "fn main() { println!() }");

        let theirs = serde_json::to_value(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: "file:///foo.rs".parse().expect("uri"),
                version: 2,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "fn main() { println!() }".to_string(),
            }],
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    #[test]
    fn did_close_matches_lsp_types() {
        let ours = did_close("file:///foo.rs");

        let theirs = serde_json::to_value(DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier {
                uri: "file:///foo.rs".parse().expect("uri"),
            },
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    #[test]
    fn did_save_matches_lsp_types() {
        let ours = did_save("file:///foo.rs");

        let theirs = serde_json::to_value(DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier {
                uri: "file:///foo.rs".parse().expect("uri"),
            },
            text: None,
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    // ── Workspace ───────────────────────────────────────────────────

    #[test]
    fn did_change_workspace_folders_matches_lsp_types() {
        let ours =
            did_change_workspace_folders(&[("file:///new", "new")], &[("file:///old", "old")]);

        let theirs = serde_json::to_value(DidChangeWorkspaceFoldersParams {
            event: WorkspaceFoldersChangeEvent {
                added: vec![LspWorkspaceFolder {
                    uri: "file:///new".parse().expect("uri"),
                    name: "new".to_string(),
                }],
                removed: vec![LspWorkspaceFolder {
                    uri: "file:///old".parse().expect("uri"),
                    name: "old".to_string(),
                }],
            },
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    #[test]
    fn workspace_symbols_matches_lsp_types() {
        let ours = workspace_symbols("MyStruct");

        let theirs = serde_json::to_value(WorkspaceSymbolParams {
            query: "MyStruct".to_string(),
            ..Default::default()
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    // ── Position-based requests ─────────────────────────────────────

    #[test]
    fn hover_matches_lsp_types() {
        let ours = hover("file:///foo.rs", 10, 5);

        let theirs = serde_json::to_value(HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: "file:///foo.rs".parse().expect("uri"),
                },
                position: lsp_types::Position {
                    line: 10,
                    character: 5,
                },
            },
            work_done_progress_params: Default::default(),
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    #[test]
    fn definition_matches_lsp_types() {
        let ours = definition("file:///foo.rs", 10, 5);

        let theirs = serde_json::to_value(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: "file:///foo.rs".parse().expect("uri"),
                },
                position: lsp_types::Position {
                    line: 10,
                    character: 5,
                },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    #[test]
    fn type_definition_matches_lsp_types() {
        let ours = type_definition("file:///foo.rs", 3, 8);

        // GotoDefinitionParams is used for typeDefinition too
        let theirs = serde_json::to_value(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: "file:///foo.rs".parse().expect("uri"),
                },
                position: lsp_types::Position {
                    line: 3,
                    character: 8,
                },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    #[test]
    fn implementation_matches_lsp_types() {
        let ours = implementation("file:///foo.rs", 5, 12);

        let theirs = serde_json::to_value(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: "file:///foo.rs".parse().expect("uri"),
                },
                position: lsp_types::Position {
                    line: 5,
                    character: 12,
                },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    #[test]
    fn prepare_rename_matches_lsp_types() {
        let ours = prepare_rename("file:///foo.rs", 7, 4);

        let theirs = serde_json::to_value(TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: "file:///foo.rs".parse().expect("uri"),
            },
            position: lsp_types::Position {
                line: 7,
                character: 4,
            },
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    #[test]
    fn references_matches_lsp_types() {
        let ours = references("file:///foo.rs", 10, 5, true);

        let theirs = serde_json::to_value(ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: "file:///foo.rs".parse().expect("uri"),
                },
                position: lsp_types::Position {
                    line: 10,
                    character: 5,
                },
            },
            context: ReferenceContext {
                include_declaration: true,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    #[test]
    fn document_symbols_matches_lsp_types() {
        let ours = document_symbols("file:///foo.rs");

        let theirs = serde_json::to_value(DocumentSymbolParams {
            text_document: TextDocumentIdentifier {
                uri: "file:///foo.rs".parse().expect("uri"),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    // ── Code actions ────────────────────────────────────────────────

    #[test]
    fn code_action_matches_lsp_types() {
        let ours = code_action("file:///foo.rs", 1, 0, 1, 10, &[]);

        let theirs = serde_json::to_value(CodeActionParams {
            text_document: TextDocumentIdentifier {
                uri: "file:///foo.rs".parse().expect("uri"),
            },
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 1,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 1,
                    character: 10,
                },
            },
            context: CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    // ── Call hierarchy ──────────────────────────────────────────────

    fn sample_call_hierarchy_item() -> CallHierarchyItem {
        CallHierarchyItem {
            name: "foo".to_string(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri: "file:///foo.rs".parse().expect("uri"),
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 1,
                    character: 0,
                },
            },
            selection_range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 3,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 6,
                },
            },
            data: None,
        }
    }

    #[test]
    fn prepare_call_hierarchy_matches_lsp_types() {
        let ours = prepare_call_hierarchy("file:///foo.rs", 5, 3);

        let theirs = serde_json::to_value(CallHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: "file:///foo.rs".parse().expect("uri"),
                },
                position: lsp_types::Position {
                    line: 5,
                    character: 3,
                },
            },
            work_done_progress_params: Default::default(),
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    #[test]
    fn incoming_calls_matches_lsp_types() {
        let item = sample_call_hierarchy_item();
        let item_value = serde_json::to_value(&item).expect("serialize item");
        let ours = incoming_calls(&item_value);

        let theirs = serde_json::to_value(CallHierarchyIncomingCallsParams {
            item,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    #[test]
    fn outgoing_calls_matches_lsp_types() {
        let item = sample_call_hierarchy_item();
        let item_value = serde_json::to_value(&item).expect("serialize item");
        let ours = outgoing_calls(&item_value);

        let theirs = serde_json::to_value(CallHierarchyOutgoingCallsParams {
            item,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    // ── Type hierarchy ──────────────────────────────────────────────

    fn sample_type_hierarchy_item() -> LspTypeHierarchyItem {
        LspTypeHierarchyItem {
            name: "MyTrait".to_string(),
            kind: SymbolKind::INTERFACE,
            tags: None,
            detail: None,
            uri: "file:///foo.rs".parse().expect("uri"),
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 5,
                    character: 0,
                },
            },
            selection_range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 6,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 13,
                },
            },
            data: None,
        }
    }

    #[test]
    fn prepare_type_hierarchy_matches_lsp_types() {
        let ours = prepare_type_hierarchy("file:///foo.rs", 2, 10);

        let theirs = serde_json::to_value(LspTypeHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: "file:///foo.rs".parse().expect("uri"),
                },
                position: lsp_types::Position {
                    line: 2,
                    character: 10,
                },
            },
            work_done_progress_params: Default::default(),
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    #[test]
    fn supertypes_matches_lsp_types() {
        let item = sample_type_hierarchy_item();
        let item_value = serde_json::to_value(&item).expect("serialize item");
        let ours = supertypes(&item_value);

        let theirs = serde_json::to_value(LspTypeHierarchySupertypesParams {
            item,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }

    #[test]
    fn subtypes_matches_lsp_types() {
        let item = sample_type_hierarchy_item();
        let item_value = serde_json::to_value(&item).expect("serialize item");
        let ours = subtypes(&item_value);

        let theirs = serde_json::to_value(LspTypeHierarchySubtypesParams {
            item,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .expect("serialize");

        assert_eq!(ours, theirs);
    }
}

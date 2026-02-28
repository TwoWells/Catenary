// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for LSP client functionality using mockls.

use anyhow::Result;
use tempfile::tempdir;

const MOCK_LANG_A: &str = "yX4Za";

#[tokio::test]
async fn test_mockls_initialize() -> Result<()> {
    let dir = tempdir()?;
    let bin = env!("CARGO_BIN_EXE_mockls");

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A],
        MOCK_LANG_A,
        catenary_mcp::session::EventBroadcaster::noop()?,
    )?;

    let result = client.initialize(&[dir.path().to_path_buf()], None).await?;

    assert!(result.capabilities.hover_provider.is_some());
    assert!(result.capabilities.definition_provider.is_some());

    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn test_mockls_initialize_workspace_folders() -> Result<()> {
    let dir = tempdir()?;
    let bin = env!("CARGO_BIN_EXE_mockls");

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A, "--workspace-folders"],
        MOCK_LANG_A,
        catenary_mcp::session::EventBroadcaster::noop()?,
    )?;

    let result = client.initialize(&[dir.path().to_path_buf()], None).await?;

    assert!(result.capabilities.hover_provider.is_some());
    assert!(client.supports_workspace_folders());

    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn test_mockls_document_lifecycle() -> Result<()> {
    let dir = tempdir()?;
    let script_path = dir.path().join(format!("lifecycle.{MOCK_LANG_A}"));
    std::fs::write(&script_path, "let MY_VAR\n")?;

    let bin = env!("CARGO_BIN_EXE_mockls");

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A],
        MOCK_LANG_A,
        catenary_mcp::session::EventBroadcaster::noop()?,
    )?;

    client.initialize(&[dir.path().to_path_buf()], None).await?;

    let uri: lsp_types::Uri = format!("file://{}", script_path.display()).parse()?;

    // Open
    client
        .did_open(lsp_types::DidOpenTextDocumentParams {
            text_document: lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: MOCK_LANG_A.to_string(),
                version: 1,
                text: "let MY_VAR\n".to_string(),
            },
        })
        .await?;

    // Change
    client
        .did_change(lsp_types::DidChangeTextDocumentParams {
            text_document: lsp_types::VersionedTextDocumentIdentifier {
                uri: uri.clone(),
                version: 2,
            },
            content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "let MY_VAR\nMY_VAR\n".to_string(),
            }],
        })
        .await?;

    // Close
    client
        .did_close(lsp_types::DidCloseTextDocumentParams {
            text_document: lsp_types::TextDocumentIdentifier { uri },
        })
        .await?;

    client.shutdown().await?;
    Ok(())
}

/// Verifies client capabilities sent during `initialize` (Gap 7).
///
/// mockls `--log-init-params` writes the full initialize request params
/// to a file. We parse it and assert the capabilities Catenary advertises.
#[tokio::test]
async fn test_client_capabilities() -> Result<()> {
    let dir = tempdir()?;
    let init_log = dir.path().join("init_params.json");
    let bin = env!("CARGO_BIN_EXE_mockls");

    let log_path = init_log.to_str().ok_or_else(|| anyhow::anyhow!("path"))?;
    let mut client = catenary_mcp::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A, "--log-init-params", log_path],
        MOCK_LANG_A,
        catenary_mcp::session::EventBroadcaster::noop()?,
    )?;

    client.initialize(&[dir.path().to_path_buf()], None).await?;

    let params_json = std::fs::read_to_string(&init_log)?;
    let params: serde_json::Value = serde_json::from_str(&params_json)?;
    let caps = &params["capabilities"];
    let text_doc = &caps["textDocument"];

    // Capabilities that MUST be present
    assert!(
        text_doc.get("synchronization").is_some(),
        "synchronization capability missing"
    );
    assert!(
        text_doc.get("definition").is_some(),
        "definition capability missing"
    );
    assert!(
        text_doc.get("references").is_some(),
        "references capability missing"
    );
    assert!(
        text_doc.get("documentSymbol").is_some(),
        "documentSymbol capability missing"
    );
    assert!(
        text_doc.get("callHierarchy").is_some(),
        "callHierarchy capability missing"
    );
    assert!(
        text_doc.get("typeHierarchy").is_some(),
        "typeHierarchy capability missing"
    );
    assert!(
        text_doc.get("implementation").is_some(),
        "implementation capability missing"
    );
    assert!(
        text_doc.get("declaration").is_some(),
        "declaration capability missing"
    );
    assert!(
        text_doc.get("typeDefinition").is_some(),
        "typeDefinition capability missing"
    );

    // `publishDiagnostics.versionSupport` must be true
    assert_eq!(
        text_doc["publishDiagnostics"]["versionSupport"], true,
        "publishDiagnostics.versionSupport must be true"
    );

    // `window.workDoneProgress` must be true
    assert_eq!(
        caps["window"]["workDoneProgress"], true,
        "window.workDoneProgress must be true"
    );

    // Capabilities that MUST NOT be present (tools were removed)
    assert!(
        text_doc.get("hover").is_none(),
        "hover capability should not be advertised"
    );
    assert!(
        text_doc.get("codeAction").is_none(),
        "codeAction capability should not be advertised"
    );
    assert!(
        text_doc.get("rename").is_none(),
        "rename capability should not be advertised"
    );
    assert!(
        text_doc.get("completion").is_none(),
        "completion capability should not be advertised"
    );
    assert!(
        text_doc.get("signatureHelp").is_none(),
        "signatureHelp capability should not be advertised"
    );
    assert!(
        text_doc.get("formatting").is_none(),
        "formatting capability should not be advertised"
    );

    client.shutdown().await?;
    Ok(())
}

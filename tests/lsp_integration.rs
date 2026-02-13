#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Integration tests for LSP client functionality.
//!
//! These tests require an LSP server to be installed. They will be skipped
//! if the required server is not available.

use anyhow::Result;
use lsp_types::WorkDoneProgressParams;
use std::process::Command;
use tempfile::tempdir;

/// Check if a command exists in PATH
fn command_exists(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Skip test if bash-language-server is not installed
macro_rules! require_bash_lsp {
    () => {
        if !command_exists("bash-language-server") {
            tracing::warn!("Skipping test: bash-language-server not installed");
            return Ok(());
        }
    };
}

/// Skip test if lua-language-server is not installed
macro_rules! require_lua_lsp {
    () => {
        if !command_exists("lua-language-server") {
            tracing::warn!("Skipping test: lua-language-server not installed");
            return Ok(());
        }
    };
}

/// Skip test if rust-analyzer is not installed and working
macro_rules! require_rust_analyzer {
    () => {
        // rust-analyzer via rustup proxy may exist but not work
        let output = Command::new("rust-analyzer").arg("--version").output();
        if output.is_err() || !output.map(|o| o.status.success()).unwrap_or(false) {
            tracing::warn!("Skipping test: rust-analyzer not installed or not working");
            return Ok(());
        }
    };
}

/// Skip test if yaml-language-server is not installed
macro_rules! require_yaml_lsp {
    () => {
        if !command_exists("yaml-language-server") {
            tracing::warn!("Skipping test: yaml-language-server not installed");
            return Ok(());
        }
    };
}

#[tokio::test]
async fn test_bash_lsp_initialize() -> Result<()> {
    require_bash_lsp!();

    let dir = tempdir()?;

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        "bash-language-server",
        &["start"],
        "shellscript",
        catenary_mcp::session::EventBroadcaster::noop()?,
    )?;

    let result = client.initialize(dir.path()).await?;

    // Verify bash-language-server capabilities
    assert!(result.capabilities.hover_provider.is_some());
    assert!(result.capabilities.definition_provider.is_some());
    assert!(result.capabilities.completion_provider.is_some());

    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn test_bash_lsp_hover() -> Result<()> {
    require_bash_lsp!();

    let dir = tempdir()?;
    let script_path = dir.path().join("test.sh");
    std::fs::write(&script_path, "#!/bin/bash\necho \"hello\"\n")?;

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        "bash-language-server",
        &["start"],
        "shellscript",
        catenary_mcp::session::EventBroadcaster::noop()?,
    )?;

    client.initialize(dir.path()).await?;

    // Open the document
    let uri: lsp_types::Uri = format!("file://{}", script_path.display()).parse()?;
    client
        .did_open(lsp_types::DidOpenTextDocumentParams {
            text_document: lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "shellscript".to_string(),
                version: 1,
                text: std::fs::read_to_string(&script_path)?,
            },
        })
        .await?;

    // Small delay to let LSP process
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Request hover on "echo"
    let hover = client
        .hover(lsp_types::HoverParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri },
                position: lsp_types::Position {
                    line: 1,
                    character: 0,
                },
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        })
        .await?;

    // echo is a builtin, should have hover info
    assert!(hover.is_some(), "Expected hover info for 'echo'");

    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn test_lua_lsp_initialize() -> Result<()> {
    require_lua_lsp!();

    let dir = tempdir()?;

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        "lua-language-server",
        &[],
        "lua",
        catenary_mcp::session::EventBroadcaster::noop()?,
    )?;

    let result = client.initialize(dir.path()).await?;

    assert!(result.capabilities.hover_provider.is_some());

    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn test_document_lifecycle() -> Result<()> {
    require_bash_lsp!();

    let dir = tempdir()?;
    let script_path = dir.path().join("lifecycle.sh");
    std::fs::write(&script_path, "#!/bin/bash\nMY_VAR=1\n")?;

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        "bash-language-server",
        &["start"],
        "shellscript",
        catenary_mcp::session::EventBroadcaster::noop()?,
    )?;

    client.initialize(dir.path()).await?;

    let uri: lsp_types::Uri = format!("file://{}", script_path.display()).parse()?;

    // Open
    client
        .did_open(lsp_types::DidOpenTextDocumentParams {
            text_document: lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "shellscript".to_string(),
                version: 1,
                text: "#!/bin/bash\nMY_VAR=1\n".to_string(),
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
                text: "#!/bin/bash\nMY_VAR=2\necho $MY_VAR\n".to_string(),
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

#[tokio::test]
async fn test_rust_analyzer_initialize() -> Result<()> {
    require_rust_analyzer!();

    let dir = tempdir()?;

    // Create a minimal Cargo.toml so rust-analyzer is happy
    std::fs::write(
        dir.path().join("Cargo.toml"),
        r#"[package]
name = "test"
version = "0.1.0"
edition = "2021"
"#,
    )?;

    // Create src directory and main.rs
    std::fs::create_dir(dir.path().join("src"))?;
    let mut client = catenary_mcp::lsp::LspClient::spawn(
        "rust-analyzer",
        &[],
        "rust",
        catenary_mcp::session::EventBroadcaster::noop()?,
    )?;

    let result = client.initialize(dir.path()).await?;

    // rust-analyzer provides hover and definition
    assert!(result.capabilities.hover_provider.is_some());
    assert!(result.capabilities.definition_provider.is_some());

    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn test_rust_analyzer_hover() -> Result<()> {
    require_rust_analyzer!();

    let dir = tempdir()?;

    std::fs::write(
        dir.path().join("Cargo.toml"),
        r#"[package]
name = "test"
version = "0.1.0"
edition = "2021"
"#,
    )?;

    std::fs::create_dir(dir.path().join("src"))?;
    let main_rs = dir.path().join("src/main.rs");
    std::fs::write(&main_rs, "fn main() {\n    let x: i32 = 42;\n}\n")?;

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        "rust-analyzer",
        &[],
        "rust",
        catenary_mcp::session::EventBroadcaster::noop()?,
    )?;

    client.initialize(dir.path()).await?;
    client.wait_ready().await;

    let uri: lsp_types::Uri = format!("file://{}", main_rs.display()).parse()?;
    client
        .did_open(lsp_types::DidOpenTextDocumentParams {
            text_document: lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "rust".to_string(),
                version: 1,
                text: std::fs::read_to_string(&main_rs)?,
            },
        })
        .await?;

    // Give rust-analyzer time to index
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Request hover on "i32"
    let hover = client
        .hover(lsp_types::HoverParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri },
                position: lsp_types::Position {
                    line: 1,
                    character: 11, // position on i32
                },
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        })
        .await?;

    assert!(hover.is_some(), "Expected hover info for 'i32'");

    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn test_yaml_lsp_initialize() -> Result<()> {
    require_yaml_lsp!();

    let dir = tempdir()?;

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        "yaml-language-server",
        &["--stdio"],
        "yaml",
        catenary_mcp::session::EventBroadcaster::noop()?,
    )?;

    let result = client.initialize(dir.path()).await?;

    assert!(result.capabilities.hover_provider.is_some());

    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn test_yaml_lsp_hover() -> Result<()> {
    require_yaml_lsp!();

    let dir = tempdir()?;
    let yaml_path = dir.path().join("test.yaml");
    std::fs::write(&yaml_path, "name: test\nversion: 1.0\n")?;

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        "yaml-language-server",
        &["--stdio"],
        "yaml",
        catenary_mcp::session::EventBroadcaster::noop()?,
    )?;

    client.initialize(dir.path()).await?;

    let uri: lsp_types::Uri = format!("file://{}", yaml_path.display()).parse()?;
    client
        .did_open(lsp_types::DidOpenTextDocumentParams {
            text_document: lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "yaml".to_string(),
                version: 1,
                text: std::fs::read_to_string(&yaml_path)?,
            },
        })
        .await?;

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Request hover - yaml-language-server may or may not return info for plain YAML
    let hover = client
        .hover(lsp_types::HoverParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri },
                position: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        })
        .await?;

    // Just verify the request succeeds (hover content depends on schema)
    drop(hover);

    client.shutdown().await?;
    Ok(())
}

//! Integration tests for LSP client functionality.
//!
//! These tests require an LSP server to be installed. They will be skipped
//! if the required server is not available.

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
            eprintln!("Skipping test: bash-language-server not installed");
            return;
        }
    };
}

/// Skip test if lua-language-server is not installed
macro_rules! require_lua_lsp {
    () => {
        if !command_exists("lua-language-server") {
            eprintln!("Skipping test: lua-language-server not installed");
            return;
        }
    };
}

/// Skip test if rust-analyzer is not installed and working
macro_rules! require_rust_analyzer {
    () => {
        // rust-analyzer via rustup proxy may exist but not work
        let output = Command::new("rust-analyzer").arg("--version").output();
        if output.is_err() || !output.unwrap().status.success() {
            eprintln!("Skipping test: rust-analyzer not installed or not working");
            return;
        }
    };
}

/// Skip test if yaml-language-server is not installed
macro_rules! require_yaml_lsp {
    () => {
        if !command_exists("yaml-language-server") {
            eprintln!("Skipping test: yaml-language-server not installed");
            return;
        }
    };
}

#[tokio::test]
async fn test_bash_lsp_initialize() {
    require_bash_lsp!();

    let dir = tempdir().unwrap();

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        "bash-language-server",
        &["start"],
        "shellscript",
        catenary_mcp::session::EventBroadcaster::noop(),
    )
    .await
    .unwrap();

    let result = client.initialize(dir.path()).await.unwrap();

    // Verify bash-language-server capabilities
    assert!(result.capabilities.hover_provider.is_some());
    assert!(result.capabilities.definition_provider.is_some());
    assert!(result.capabilities.completion_provider.is_some());

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_bash_lsp_hover() {
    require_bash_lsp!();

    let dir = tempdir().unwrap();
    let script_path = dir.path().join("test.sh");
    std::fs::write(&script_path, "#!/bin/bash\necho \"hello\"\n").unwrap();

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        "bash-language-server",
        &["start"],
        "shellscript",
        catenary_mcp::session::EventBroadcaster::noop(),
    )
    .await
    .unwrap();

    client.initialize(dir.path()).await.unwrap();

    // Open the document
    let uri: lsp_types::Uri = format!("file://{}", script_path.display()).parse().unwrap();
    client
        .did_open(lsp_types::DidOpenTextDocumentParams {
            text_document: lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "shellscript".to_string(),
                version: 1,
                text: std::fs::read_to_string(&script_path).unwrap(),
            },
        })
        .await
        .unwrap();

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
            work_done_progress_params: Default::default(),
        })
        .await
        .unwrap();

    // echo is a builtin, should have hover info
    assert!(hover.is_some(), "Expected hover info for 'echo'");

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_lua_lsp_initialize() {
    require_lua_lsp!();

    let dir = tempdir().unwrap();

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        "lua-language-server",
        &[],
        "lua",
        catenary_mcp::session::EventBroadcaster::noop(),
    )
    .await
    .unwrap();

    let result = client.initialize(dir.path()).await.unwrap();

    assert!(result.capabilities.hover_provider.is_some());

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_document_lifecycle() {
    require_bash_lsp!();

    let dir = tempdir().unwrap();
    let script_path = dir.path().join("lifecycle.sh");
    std::fs::write(&script_path, "#!/bin/bash\nMY_VAR=1\n").unwrap();

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        "bash-language-server",
        &["start"],
        "shellscript",
        catenary_mcp::session::EventBroadcaster::noop(),
    )
    .await
    .unwrap();

    client.initialize(dir.path()).await.unwrap();

    let uri: lsp_types::Uri = format!("file://{}", script_path.display()).parse().unwrap();

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
        .await
        .unwrap();

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
        .await
        .unwrap();

    // Close
    client
        .did_close(lsp_types::DidCloseTextDocumentParams {
            text_document: lsp_types::TextDocumentIdentifier { uri },
        })
        .await
        .unwrap();

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_rust_analyzer_initialize() {
    require_rust_analyzer!();

    let dir = tempdir().unwrap();

    // Create a minimal Cargo.toml so rust-analyzer is happy
    std::fs::write(
        dir.path().join("Cargo.toml"),
        r#"[package]
name = "test"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();

    // Create src directory and main.rs
    std::fs::create_dir(dir.path().join("src")).unwrap();
    let mut client = catenary_mcp::lsp::LspClient::spawn(
        "rust-analyzer",
        &[],
        "rust",
        catenary_mcp::session::EventBroadcaster::noop(),
    )
    .await
    .unwrap();

    let result = client.initialize(dir.path()).await.unwrap();

    // rust-analyzer provides hover and definition
    assert!(result.capabilities.hover_provider.is_some());
    assert!(result.capabilities.definition_provider.is_some());

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_rust_analyzer_hover() {
    require_rust_analyzer!();

    let dir = tempdir().unwrap();

    std::fs::write(
        dir.path().join("Cargo.toml"),
        r#"[package]
name = "test"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();

    std::fs::create_dir(dir.path().join("src")).unwrap();
    let main_rs = dir.path().join("src/main.rs");
    std::fs::write(&main_rs, "fn main() {\n    let x: i32 = 42;\n}\n").unwrap();

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        "rust-analyzer",
        &[],
        "rust",
        catenary_mcp::session::EventBroadcaster::noop(),
    )
    .await
    .unwrap();

    client.initialize(dir.path()).await.unwrap();

    let uri: lsp_types::Uri = format!("file://{}", main_rs.display()).parse().unwrap();
    client
        .did_open(lsp_types::DidOpenTextDocumentParams {
            text_document: lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "rust".to_string(),
                version: 1,
                text: std::fs::read_to_string(&main_rs).unwrap(),
            },
        })
        .await
        .unwrap();

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
            work_done_progress_params: Default::default(),
        })
        .await
        .unwrap();

    assert!(hover.is_some(), "Expected hover info for 'i32'");

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_yaml_lsp_initialize() {
    require_yaml_lsp!();

    let dir = tempdir().unwrap();

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        "yaml-language-server",
        &["--stdio"],
        "yaml",
        catenary_mcp::session::EventBroadcaster::noop(),
    )
    .await
    .unwrap();

    let result = client.initialize(dir.path()).await.unwrap();

    assert!(result.capabilities.hover_provider.is_some());

    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_yaml_lsp_hover() {
    require_yaml_lsp!();

    let dir = tempdir().unwrap();
    let yaml_path = dir.path().join("test.yaml");
    std::fs::write(&yaml_path, "name: test\nversion: 1.0\n").unwrap();

    let mut client = catenary_mcp::lsp::LspClient::spawn(
        "yaml-language-server",
        &["--stdio"],
        "yaml",
        catenary_mcp::session::EventBroadcaster::noop(),
    )
    .await
    .unwrap();

    client.initialize(dir.path()).await.unwrap();

    let uri: lsp_types::Uri = format!("file://{}", yaml_path.display()).parse().unwrap();
    client
        .did_open(lsp_types::DidOpenTextDocumentParams {
            text_document: lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "yaml".to_string(),
                version: 1,
                text: std::fs::read_to_string(&yaml_path).unwrap(),
            },
        })
        .await
        .unwrap();

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
            work_done_progress_params: Default::default(),
        })
        .await;

    // Just verify the request succeeds (hover content depends on schema)
    assert!(hover.is_ok());

    client.shutdown().await.unwrap();
}

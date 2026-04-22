// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for the `glob` tool (directory/file/pattern modes).

mod common;

use anyhow::{Context, Result};
use serde_json::json;

use common::BridgeProcess;

const MOCK_LANG_A: &str = "yX4Za";

/// Spawns a bridge with an optional custom LSP arg.
///
/// If `lsp_args` is `None`, uses mockls for `MOCK_LANG_A`.
fn spawn_bridge(root: &str, lsp_args: Option<&str>) -> Result<BridgeProcess> {
    let default_lsp = common::mockls_lsp_arg(MOCK_LANG_A, "");
    let lsp = lsp_args.unwrap_or(&default_lsp);
    BridgeProcess::spawn(&[lsp], root)
}

/// Spawns a bridge with multiple roots and an optional custom LSP arg.
fn spawn_bridge_multi_root(roots: &[&str], lsp_args: Option<&str>) -> Result<BridgeProcess> {
    let default_lsp = common::mockls_lsp_arg(MOCK_LANG_A, "");
    let lsp = lsp_args.unwrap_or(&default_lsp);
    BridgeProcess::spawn_multi_root(&[lsp], roots)
}

#[test]
fn test_glob_directory_basic() -> Result<()> {
    let temp = tempfile::tempdir()?;
    std::fs::write(temp.path().join("file1.txt"), "content")?;
    std::fs::create_dir(temp.path().join("subdir"))?;
    std::fs::write(temp.path().join("subdir/file2.rs"), "fn main() {}")?;

    let mut bridge = spawn_bridge(temp.path().to_str().context("invalid path")?, None)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "glob",
            "arguments": {
                "pattern": temp.path().to_str().context("invalid path")?
            }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(result["isError"].is_null() || result["isError"] == false);

    let content = result["content"][0]["text"]
        .as_str()
        .context("Missing text in content")?;

    assert!(
        content.contains("file1.txt"),
        "Should list file1.txt, got:\n{content}"
    );
    assert!(
        content.contains("subdir/"),
        "Should list subdir/, got:\n{content}"
    );
    Ok(())
}

#[test]
fn test_glob_directory_symbols() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let script = temp.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(&script, "struct Config\nenum Mode\nconst MAX_SIZE\n")?;

    let mut bridge = spawn_bridge(temp.path().to_str().context("invalid path")?, None)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "glob",
            "arguments": {
                "pattern": temp.path().to_str().context("invalid path")?
            }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    let content = result["content"][0]["text"]
        .as_str()
        .context("Missing text in content")?;

    assert!(
        content.contains(&format!("types.{MOCK_LANG_A}")),
        "Should list the file, got:\n{content}"
    );
    assert!(
        content.contains("Config"),
        "Should contain Config symbol, got:\n{content}"
    );
    assert!(
        content.contains("Mode"),
        "Should contain Mode symbol, got:\n{content}"
    );
    Ok(())
}

/// Verifies that glob returns outline symbols for a single file,
/// filtering to outline kinds only.
#[test]
fn test_glob_file_outline() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let script = temp.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(
        &script,
        "struct Config\nenum Mode\nconst MAX_SIZE\nfn do_work\n",
    )?;

    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let lsp = format!("{MOCK_LANG_A}:{mockls_bin} {MOCK_LANG_A}");

    let mut bridge = spawn_bridge(temp.path().to_str().context("invalid path")?, Some(&lsp))?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "glob",
            "arguments": {
                "pattern": script.to_str().context("file path")?
            }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    let content = result["content"][0]["text"]
        .as_str()
        .context("Missing text in content")?;

    // Outline kinds should be present
    assert!(
        content.contains("Config"),
        "Should contain Config symbol, got:\n{content}"
    );
    assert!(
        content.contains("Struct"),
        "Config should have Struct kind, got:\n{content}"
    );
    assert!(
        content.contains("Mode"),
        "Should contain Mode symbol, got:\n{content}"
    );
    assert!(
        content.contains("Enum"),
        "Mode should have Enum kind, got:\n{content}"
    );
    assert!(
        content.contains("MAX_SIZE"),
        "Should contain MAX_SIZE symbol, got:\n{content}"
    );
    assert!(
        content.contains("Constant"),
        "MAX_SIZE should have Constant kind, got:\n{content}"
    );

    // Function kind should be excluded from outline
    assert!(
        !content.contains("do_work"),
        "Function 'do_work' should be excluded from outline, got:\n{content}"
    );

    // Line numbers should be present
    assert!(
        content.contains("L1"),
        "Should contain L1 line number, got:\n{content}"
    );
    assert!(
        content.contains("L2"),
        "Should contain L2 line number, got:\n{content}"
    );

    // Line count header
    assert!(
        content.contains("(4 lines)"),
        "Should show line count, got:\n{content}"
    );
    Ok(())
}

#[test]
fn test_glob_directory_explicit_path() -> Result<()> {
    // When an explicit path is given, even in multi-root mode, only that path is shown
    let dir_a = tempfile::tempdir()?;
    let dir_b = tempfile::tempdir()?;

    std::fs::write(dir_a.path().join("only_a.txt"), "a")?;
    std::fs::write(dir_b.path().join("only_b.txt"), "b")?;

    let root_a = dir_a.path().to_str().context("invalid path A")?;
    let root_b = dir_b.path().to_str().context("invalid path B")?;

    let mut bridge = spawn_bridge_multi_root(&[root_a, root_b], None)?;
    bridge.initialize()?;

    // Request glob with explicit path pointing to root A only
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "tools/call",
        "params": {
            "name": "glob",
            "arguments": {
                "pattern": root_a
            }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "glob with explicit path failed: {response:?}"
    );

    let content = result["content"][0]["text"]
        .as_str()
        .context("Missing text in content")?;

    assert!(
        content.contains("only_a.txt"),
        "Should contain only_a.txt from explicit path, got:\n{content}"
    );
    assert!(
        !content.contains("only_b.txt"),
        "Should NOT contain only_b.txt when explicit path is root A, got:\n{content}"
    );

    Ok(())
}

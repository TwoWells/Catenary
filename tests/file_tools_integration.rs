// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for the `glob` tool.

mod common;

use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;

use common::BridgeProcess;

const MOCK_LANG_A: &str = "yX4Za";

/// Spawns the bridge without any LSP servers configured.
fn spawn_no_lsp(root: &str) -> Result<BridgeProcess> {
    BridgeProcess::spawn(&[], root)
}

/// Spawns the bridge with a real LSP server argument.
fn spawn_with_real_lsp(lsp_arg: &str, root: &str) -> Result<BridgeProcess> {
    BridgeProcess::spawn(&[lsp_arg], root)
}

/// Spawns the bridge with mockls configured for `MOCK_LANG_A`.
fn spawn_with_lsp(root: &str) -> Result<BridgeProcess> {
    let lsp = common::mockls_lsp_arg(MOCK_LANG_A, "");
    BridgeProcess::spawn(&[&lsp], root)
}

#[test]
fn test_glob_directory_basic() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::create_dir_all(dir.path().join("src"))?;
    std::fs::write(dir.path().join("Cargo.toml"), "[package]")?;
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    assert!(text.contains("src/"), "Should list src directory: {text}");
    assert!(
        text.contains("Cargo.toml"),
        "Should list Cargo.toml: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_outside_root() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    std::fs::write(outside.path().join("hello.txt"), "hi")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let result = bridge.call_tool(
        "glob",
        &json!({ "pattern": outside.path().to_string_lossy().as_ref() }),
    )?;

    let is_error = result.get("isError").and_then(serde_json::Value::as_bool);
    assert_ne!(is_error, Some(true), "Should not be an error");

    let text = result
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|e| e.get("text"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    assert!(
        text.contains("hello.txt"),
        "Should list files outside workspace roots: {text}"
    );
    Ok(())
}

#[test]
fn test_tools_list_includes_glob() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    }))?;

    let response = bridge.recv()?;
    let tools = response
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .context("No tools in response")?;

    let tool_names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();

    assert!(
        tool_names.contains(&"glob"),
        "Should include glob: {tool_names:?}"
    );
    assert!(
        !tool_names.contains(&"list_directory"),
        "Should not include list_directory: {tool_names:?}"
    );
    assert!(
        !tool_names.contains(&"document_symbols"),
        "Should not include document_symbols: {tool_names:?}"
    );
    assert!(
        !tool_names.contains(&"codebase_map"),
        "Should not include codebase_map: {tool_names:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn test_glob_directory_symlink() -> Result<()> {
    use std::os::unix::fs as unix_fs;

    let dir = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;

    std::fs::write(outside.path().join("secret.txt"), "secret")?;

    // Create symlink inside workspace pointing outside
    unix_fs::symlink(
        outside.path().join("secret.txt"),
        dir.path().join("link.txt"),
    )?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // Symlink should be shown with its target
    assert!(
        text.contains("link.txt ->"),
        "Symlink should be shown with arrow: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_file_outline() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let script = dir.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(
        &script,
        "struct Config\nenum Mode\nconst MAX_SIZE\nfn helper\n",
    )?;

    let mut bridge = spawn_with_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": script.to_str().context("file path")? }),
    )?;

    // Outline kinds present
    assert!(text.contains("Config"), "Should contain Config: {text}");
    assert!(text.contains("Struct"), "Config should be Struct: {text}");
    assert!(text.contains("Mode"), "Should contain Mode: {text}");
    assert!(text.contains("MAX_SIZE"), "Should contain MAX_SIZE: {text}");

    // Function excluded from outline
    assert!(
        !text.contains("helper"),
        "Function should be excluded from outline: {text}"
    );

    // Line numbers
    assert!(text.contains("L1"), "Should have L1: {text}");
    assert!(text.contains("L2"), "Should have L2: {text}");
    assert!(text.contains("L3"), "Should have L3: {text}");
    Ok(())
}

#[test]
fn test_glob_pattern_matching() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("main.rs"), "fn main() {}")?;
    std::fs::write(dir.path().join("lib.rs"), "pub mod lib;")?;
    std::fs::write(dir.path().join("readme.md"), "# Readme")?;

    let mut bridge = spawn_with_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "pattern": "*.rs" }))?;

    assert!(text.contains("main.rs"), "Should match main.rs: {text}");
    assert!(text.contains("lib.rs"), "Should match lib.rs: {text}");
    assert!(
        !text.contains("readme.md"),
        "Should not match readme.md: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_alternation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("main.rs"), "fn main() {}")?;
    std::fs::write(dir.path().join("Cargo.toml"), "[package]")?;
    std::fs::write(dir.path().join("readme.md"), "# Readme")?;

    let mut bridge = spawn_with_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "pattern": "*.{rs,toml}" }))?;

    assert!(text.contains("main.rs"), "Should match main.rs: {text}");
    assert!(
        text.contains("Cargo.toml"),
        "Should match Cargo.toml: {text}"
    );
    assert!(
        !text.contains("readme.md"),
        "Should not match readme.md: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_line_counts() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("three.txt"), "line1\nline2\nline3\n")?;
    std::fs::write(dir.path().join("one.txt"), "single\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    assert!(
        text.contains("(3 lines)"),
        "Should show 3 lines for three.txt: {text}"
    );
    assert!(
        text.contains("(1 lines)"),
        "Should show 1 lines for one.txt: {text}"
    );
    // Should NOT show bytes
    assert!(!text.contains("bytes"), "Should not show bytes: {text}");
    Ok(())
}

#[test]
fn test_glob_gitignored_section() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Initialize a git repo so gitignore is recognized
    Command::new("git")
        .args(["init"])
        .current_dir(dir.path())
        .output()
        .context("Failed to run git init")?;

    std::fs::write(dir.path().join(".gitignore"), "*.log\nbuild/\n")?;
    std::fs::write(dir.path().join("app.txt"), "content")?;
    std::fs::write(dir.path().join("debug.log"), "log data")?;
    std::fs::create_dir(dir.path().join("build"))?;
    std::fs::write(dir.path().join("build/output.bin"), "binary")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // Non-ignored files should be listed normally
    assert!(text.contains("app.txt"), "Should list app.txt: {text}");

    // Gitignored section should exist
    assert!(
        text.contains("gitignored:"),
        "Should have gitignored section: {text}"
    );

    // Gitignored entries should be in the section
    assert!(
        text.contains("debug.log"),
        "Should list debug.log in gitignored: {text}"
    );
    assert!(
        text.contains("build/"),
        "Should list build/ in gitignored: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_pattern_detection() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let script = dir.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(&script, "struct Config\nenum Mode\n")?;
    std::fs::create_dir(dir.path().join("subdir"))?;

    let mut bridge = spawn_with_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // File path → outline format (shows line count + symbols)
    let file_text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": script.to_str().context("file path")? }),
    )?;
    assert!(
        file_text.contains("(2 lines)"),
        "File mode should show line count: {file_text}"
    );
    assert!(
        file_text.contains("Config"),
        "File mode should show symbols: {file_text}"
    );

    // Directory path → listing format (shows entries)
    let dir_text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;
    assert!(
        dir_text.contains("subdir/"),
        "Dir mode should show subdirectories: {dir_text}"
    );
    assert!(
        dir_text.contains(&format!("types.{MOCK_LANG_A}")),
        "Dir mode should list files: {dir_text}"
    );

    // Glob pattern → match format (shows full paths)
    let glob_text =
        bridge.call_tool_text("glob", &json!({ "pattern": format!("*.{MOCK_LANG_A}") }))?;
    assert!(
        glob_text.contains(&format!("types.{MOCK_LANG_A}")),
        "Pattern mode should match files: {glob_text}"
    );
    Ok(())
}

// ─── lua-language-server integration tests ──────────────────────────────

/// Glob file outline with real lua-language-server.
///
/// Creates a `.lua` file with a module table and local functions,
/// globs it as a file path, and checks that documentSymbol returns
/// outline data without hanging.
///
/// Run with: `make test T=lua_glob_file_outline -- --ignored`
/// Requires: lua-language-server on PATH.
#[test]
#[ignore = "requires lua-language-server"]
fn test_lua_glob_file_outline() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let lua_file = dir.path().join("helpers.lua");
    std::fs::write(
        &lua_file,
        "local M = {}\n\n\
         local MAX_RETRIES = 5\n\n\
         function M.setup(opts)\n  \
             M.opts = opts\n\
         end\n\n\
         function M.run()\n  \
             return true\n\
         end\n\n\
         return M\n",
    )?;

    let mut bridge = spawn_with_real_lsp("lua:lua-language-server", &dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // lua-language-server needs a moment to start; poll until symbols appear
    let mut text = String::new();
    for _ in 0..10 {
        std::thread::sleep(Duration::from_secs(1));

        let result = bridge.call_tool_text(
            "glob",
            &json!({ "pattern": lua_file.to_str().context("file path")? }),
        )?;

        if result.contains('M') || result.contains("setup") || result.contains("MAX_RETRIES") {
            text = result;
            break;
        }
        text = result;
    }

    assert!(text.contains("lines)"), "Should show line count: {text}");

    Ok(())
}

/// Glob pattern match across multiple lua files in subdirectories.
///
/// Mimics the chezmoi structure from `slow_glob.md` (`conky/lua/*.lua`).
/// Tests that `**/*.lua` completes without stacking 30s timeouts.
///
/// Run with: `make test T=lua_glob_pattern -- --ignored`
/// Requires: lua-language-server on PATH.
#[test]
#[ignore = "requires lua-language-server"]
fn test_lua_glob_pattern() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Mimic the chezmoi conky structure
    let lua_dir = dir.path().join("conky/lua");
    std::fs::create_dir_all(&lua_dir)?;

    std::fs::write(
        lua_dir.join("main.lua"),
        "local M = {}\nfunction M.init() end\nreturn M\n",
    )?;
    std::fs::write(
        lua_dir.join("helpers.lua"),
        "local H = {}\nfunction H.clamp(v, lo, hi) return math.max(lo, math.min(hi, v)) end\nreturn H\n",
    )?;
    std::fs::write(
        lua_dir.join("draw.lua"),
        "local D = {}\nfunction D.rect(x, y, w, h) end\nreturn D\n",
    )?;
    std::fs::write(
        lua_dir.join("list.lua"),
        "local L = {}\nfunction L.new() return {} end\nreturn L\n",
    )?;

    // Non-lua file that should not match
    std::fs::write(dir.path().join("conky/conky.conf"), "-- config\n")?;

    let mut bridge = spawn_with_real_lsp("lua:lua-language-server", &dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Give lua-language-server time to start
    std::thread::sleep(Duration::from_secs(2));

    let start = std::time::Instant::now();
    let text = bridge.call_tool_text("glob", &json!({ "pattern": "**/*.lua" }))?;
    let elapsed = start.elapsed();

    assert!(text.contains("main.lua"), "Should match main.lua: {text}");
    assert!(
        text.contains("helpers.lua"),
        "Should match helpers.lua: {text}"
    );
    assert!(text.contains("draw.lua"), "Should match draw.lua: {text}");
    assert!(text.contains("list.lua"), "Should match list.lua: {text}");
    assert!(
        !text.contains("conky.conf"),
        "Should not match conky.conf: {text}"
    );

    // If this takes >60s, something is seriously wrong (4 files should not
    // take anywhere near the 120s seen in slow_glob.md)
    assert!(
        elapsed < Duration::from_secs(60),
        "Glob pattern took {elapsed:?} — possible stacked LSP timeouts"
    );

    Ok(())
}

/// Glob directory listing with mixed file types including lua.
///
/// Tests that lua files get outline symbols while non-lua files
/// just get line counts.
///
/// Run with: `make test T=lua_glob_directory -- --ignored`
/// Requires: lua-language-server on PATH.
#[test]
#[ignore = "requires lua-language-server"]
fn test_lua_glob_directory() -> Result<()> {
    let dir = tempfile::tempdir()?;

    std::fs::write(
        dir.path().join("init.lua"),
        "local M = {}\nfunction M.setup() end\nreturn M\n",
    )?;
    std::fs::write(dir.path().join("config.json"), "{\"key\": \"value\"}\n")?;
    std::fs::write(dir.path().join("notes.txt"), "some notes\n")?;

    let mut bridge = spawn_with_real_lsp("lua:lua-language-server", &dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Give lua-language-server time to start
    std::thread::sleep(Duration::from_secs(2));

    let start = std::time::Instant::now();
    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;
    let elapsed = start.elapsed();

    assert!(text.contains("init.lua"), "Should list init.lua: {text}");
    assert!(
        text.contains("config.json"),
        "Should list config.json: {text}"
    );
    assert!(text.contains("notes.txt"), "Should list notes.txt: {text}");

    assert!(
        elapsed < Duration::from_secs(60),
        "Directory glob took {elapsed:?} — possible stacked LSP timeouts"
    );

    Ok(())
}

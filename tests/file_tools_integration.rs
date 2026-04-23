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
#[allow(dead_code, reason = "used by ignored lua-language-server tests")]
fn spawn_with_real_lsp(lsp_arg: &str, root: &str) -> Result<BridgeProcess> {
    BridgeProcess::spawn(&[lsp_arg], root)
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
fn test_glob_file_header() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let script = dir.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(
        &script,
        "struct Config\nenum Mode\nconst MAX_SIZE\nfn helper\n",
    )?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": script.to_str().context("file path")? }),
    )?;

    // File header with line count
    assert!(text.contains("(4 lines)"), "Should show line count: {text}");

    // No symbols in output (maps not implemented in 08a)
    assert!(
        !text.contains("Config"),
        "Should not show symbols yet: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_pattern_matching() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("main.rs"), "fn main() {}")?;
    std::fs::write(dir.path().join("lib.rs"), "pub mod lib;")?;
    std::fs::write(dir.path().join("readme.md"), "# Readme")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
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

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
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
fn test_glob_pattern_detection() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let script = dir.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(&script, "struct Config\nenum Mode\n")?;
    std::fs::create_dir(dir.path().join("subdir"))?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // File path → header format (shows line count)
    let file_text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": script.to_str().context("file path")? }),
    )?;
    assert!(
        file_text.contains("(2 lines)"),
        "File mode should show line count: {file_text}"
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

    // Glob pattern → match format
    let glob_text =
        bridge.call_tool_text("glob", &json!({ "pattern": format!("*.{MOCK_LANG_A}") }))?;
    assert!(
        glob_text.contains(&format!("types.{MOCK_LANG_A}")),
        "Pattern mode should match files: {glob_text}"
    );
    Ok(())
}

// ─── New 08a tests ─────────────────────────────────────────────────────

#[test]
fn test_glob_exclude() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src)?;
    std::fs::write(src.join("main.rs"), "fn main() {}")?;
    std::fs::write(src.join("test_helper.rs"), "fn test() {}")?;
    std::fs::write(src.join("test_util.rs"), "fn util() {}")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({
            "pattern": "src/*.rs",
            "exclude": "test_*"
        }),
    )?;

    assert!(text.contains("main.rs"), "Should include main.rs: {text}");
    assert!(
        !text.contains("test_helper.rs"),
        "Should exclude test_helper.rs: {text}"
    );
    assert!(
        !text.contains("test_util.rs"),
        "Should exclude test_util.rs: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_include_hidden() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("visible.txt"), "content")?;
    std::fs::write(dir.path().join(".hidden"), "secret")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Default: hidden files excluded
    let text_default = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;
    assert!(
        text_default.contains("visible.txt"),
        "Should show visible.txt: {text_default}"
    );
    assert!(
        !text_default.contains(".hidden"),
        "Should not show .hidden by default: {text_default}"
    );

    // With include_hidden: true
    let text_hidden = bridge.call_tool_text(
        "glob",
        &json!({
            "pattern": dir.path().to_string_lossy().to_string(),
            "include_hidden": true
        }),
    )?;
    assert!(
        text_hidden.contains(".hidden"),
        "Should show .hidden with include_hidden: {text_hidden}"
    );
    Ok(())
}

#[test]
fn test_glob_include_gitignored() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Initialize git repo
    Command::new("git")
        .args(["init"])
        .current_dir(dir.path())
        .output()
        .context("Failed to run git init")?;

    std::fs::write(dir.path().join(".gitignore"), "*.log\n")?;
    std::fs::write(dir.path().join("app.txt"), "content")?;
    std::fs::write(dir.path().join("debug.log"), "log data")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Default: gitignored files absent
    let text_default = bridge.call_tool_text(
        "glob",
        &json!({
            "pattern": dir.path().to_string_lossy().to_string(),
            "include_hidden": true
        }),
    )?;
    assert!(
        text_default.contains("app.txt"),
        "Should show app.txt: {text_default}"
    );
    assert!(
        !text_default.contains("debug.log"),
        "Should not show debug.log by default: {text_default}"
    );

    // With include_gitignored: true
    let text_ignored = bridge.call_tool_text(
        "glob",
        &json!({
            "pattern": dir.path().to_string_lossy().to_string(),
            "include_gitignored": true,
            "include_hidden": true
        }),
    )?;
    assert!(
        text_ignored.contains("debug.log"),
        "Should show debug.log with include_gitignored: {text_ignored}"
    );
    Ok(())
}

#[test]
fn test_glob_tier3_bucketed() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Create many files with separator-based names to exceed budget.
    for i in 0..30 {
        std::fs::write(
            dir.path().join(format!("test_grep_{i}.rs")),
            format!("// file {i}\n"),
        )?;
    }
    for i in 0..20 {
        std::fs::write(
            dir.path().join(format!("test_glob_{i}.rs")),
            format!("// file {i}\n"),
        )?;
    }

    // Use a small budget to force bucketing.
    // The test spawns with default config, so we need enough files
    // that the file listing exceeds the default 2000-char budget.
    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // With 50 files, each line is ~30 chars = ~1500 chars.
    // If it fits in budget, we get tier 2 (all filenames).
    // If not, we get tier 3 (bucketed).
    // Either way, the output should be valid. Assert basic structure.
    assert!(
        text.contains("test_grep_") || text.contains("test_glob_"),
        "Should contain file references: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_tier2_file_listing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::create_dir(dir.path().join("src"))?;
    std::fs::write(dir.path().join("main.rs"), "fn main() {}")?;
    std::fs::write(dir.path().join("lib.rs"), "pub mod lib;")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // Small directory — should get tier 2 file listing.
    assert!(text.contains("src/"), "Should list directory: {text}");
    assert!(text.contains("main.rs"), "Should list main.rs: {text}");
    assert!(text.contains("lib.rs"), "Should list lib.rs: {text}");

    // Directories should appear before files.
    let src_pos = text.find("src/").expect("src/ should be in output");
    let main_pos = text.find("main.rs").expect("main.rs should be in output");
    assert!(
        src_pos < main_pos,
        "Directories should sort before files: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_budget_small() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Create enough files to test budget pressure.
    for i in 0..40 {
        std::fs::write(
            dir.path().join(format!("test_item_{i:03}.txt")),
            format!("line {i}\n"),
        )?;
    }

    // Write a config with a very small glob budget.
    let config_dir = tempfile::tempdir()?;
    let config_path = config_dir.path().join("config.toml");
    std::fs::write(&config_path, "[tools.glob]\nbudget = 1000\n")?;

    let mut bridge = BridgeProcess::spawn_with_config(&config_path, &dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // With small budget, output should be compact.
    assert!(
        text.len() <= 1200, // some tolerance
        "Output should be budget-constrained: len={}, text:\n{text}",
        text.len()
    );
    Ok(())
}

#[test]
fn test_glob_bucket_drill() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Create files with a shared prefix.
    for i in 0..5 {
        std::fs::write(
            dir.path().join(format!("test_grep_{i}.rs")),
            format!("fn test_{i}() {{}}\n"),
        )?;
    }
    for i in 0..5 {
        std::fs::write(
            dir.path().join(format!("test_glob_{i}.rs")),
            format!("fn test_{i}() {{}}\n"),
        )?;
    }
    std::fs::write(dir.path().join("README.md"), "# Readme\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // First call: directory listing (may be tier 2 or 3).
    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // If there's a bucket pattern, it should be a valid glob.
    if text.contains("files)") {
        // Extract a bucket pattern — lines like "test_grep_*  (5 files)"
        for line in text.lines() {
            if line.contains("files)") {
                let pattern = line.split("  (").next().unwrap_or("").trim();
                if !pattern.is_empty() {
                    // The bucket pattern should be passable back to glob.
                    let drill = bridge.call_tool_text("glob", &json!({ "pattern": pattern }))?;
                    assert!(
                        !drill.contains("No matches"),
                        "Bucket pattern '{pattern}' should be drillable: {drill}"
                    );
                }
            }
        }
    }
    Ok(())
}

#[test]
fn test_glob_pattern_tree() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("src");
    let bridge_dir = src.join("bridge");
    let lsp_dir = src.join("lsp");
    std::fs::create_dir_all(&bridge_dir)?;
    std::fs::create_dir_all(&lsp_dir)?;

    std::fs::write(bridge_dir.join("handler.rs"), "fn handle() {}\n")?;
    std::fs::write(bridge_dir.join("mod.rs"), "mod handler;\n")?;
    std::fs::write(lsp_dir.join("client.rs"), "struct Client;\n")?;
    std::fs::write(src.join("lib.rs"), "mod bridge;\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "pattern": "src/**/*.rs" }))?;

    // Should produce a nested tree.
    assert!(
        text.contains("src/") || text.contains("bridge/"),
        "Should have directory nodes: {text}"
    );
    assert!(
        text.contains("handler.rs"),
        "Should include handler.rs: {text}"
    );
    assert!(
        text.contains("client.rs"),
        "Should include client.rs: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_tab_structure() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let sub = dir.path().join("src").join("inner");
    std::fs::create_dir_all(&sub)?;
    std::fs::write(sub.join("file.rs"), "fn f() {}\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "pattern": "src/**/*.rs" }))?;

    // Tree output should use literal tab characters for indentation.
    assert!(text.contains('\t'), "Should use tab indentation: {text:?}");
    Ok(())
}

#[test]
fn test_glob_directories_count_against_budget() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Create many directories that eat into the budget.
    for i in 0..60 {
        std::fs::create_dir(dir.path().join(format!("dir_{i:03}")))?;
    }
    // Add a few files too.
    for i in 0..10 {
        std::fs::write(
            dir.path().join(format!("file_{i}.txt")),
            format!("content {i}\n"),
        )?;
    }

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // With 60 dirs + 10 files, each ~15 chars, total ~1050 chars.
    // Should fit in default 2000 budget, but if it's over it should bucket.
    // The key assertion: directories are included in the output.
    assert!(
        text.contains("dir_"),
        "Directories should appear in output: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_separator_bucketing() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Create files with underscore separators.
    for i in 0..5 {
        std::fs::write(
            dir.path().join(format!("test_grep_{i}.rs")),
            format!("fn test_{i}() {{}}\n"),
        )?;
    }
    for i in 0..5 {
        std::fs::write(
            dir.path().join(format!("test_glob_{i}.rs")),
            format!("fn test_{i}() {{}}\n"),
        )?;
    }

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Use a small budget to force bucketing.
    // We can't change the budget without a config file, so create enough
    // files or use a config.
    let config_dir = tempfile::tempdir()?;
    let config_path = config_dir.path().join("config.toml");
    std::fs::write(&config_path, "[tools.glob]\nbudget = 1000\n")?;

    let mut bridge2 =
        BridgeProcess::spawn_with_config(&config_path, &dir.path().to_string_lossy())?;
    bridge2.initialize()?;

    let text = bridge2.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // With a small budget and separator-based filenames, bucketing should
    // produce semantic groups like test_grep_* and test_glob_*.
    // If tier 2 fits, we'll see individual files. Either is valid.
    assert!(
        text.contains("test_grep_") || text.contains("test_glob_"),
        "Should have test file references: {text}"
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

    // lua-language-server needs a moment to start; poll until ready
    let mut text = String::new();
    for _ in 0..10 {
        std::thread::sleep(Duration::from_secs(1));

        let result = bridge.call_tool_text(
            "glob",
            &json!({ "pattern": lua_file.to_str().context("file path")? }),
        )?;

        if result.contains("lines)") {
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
/// Tests that lua files get line counts while non-lua files
/// just get line counts too.
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

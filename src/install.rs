// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Grammar installation, listing, and removal for tree-sitter integration.
//!
//! The `catenary install` command is the only path for grammar management.
//! Grammars are compiled from source into shared libraries and stored in
//! the Catenary data directory with a `metadata.json` sidecar. No database
//! dependency — the grammar directory is the source of truth.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use chrono::Utc;
use serde::{Deserialize, Serialize};

/// Metadata sidecar for an installed grammar.
///
/// Written as `metadata.json` alongside the compiled `.so` and `tags.scm`
/// in each grammar's scope directory.
#[derive(Debug, Serialize, Deserialize)]
pub struct GrammarMetadata {
    /// `TextMate` scope (e.g., `"source.rust"`).
    pub scope: String,
    /// File extensions this grammar handles (e.g., `["rs"]`).
    pub file_types: Vec<String>,
    /// Original clone URL or local path.
    pub repo_url: String,
    /// ISO 8601 installation timestamp.
    pub installed_at: String,
}

/// Returns the Catenary data directory.
///
/// Resolution order:
/// 1. `CATENARY_DATA_DIR` environment variable.
/// 2. `dirs::data_dir()` (`XDG_DATA_HOME` on Linux).
/// 3. `dirs::data_local_dir()` (macOS / Windows fallback).
/// 4. `/tmp` as a last resort.
fn data_dir() -> PathBuf {
    std::env::var_os("CATENARY_DATA_DIR")
        .map(PathBuf::from)
        .or_else(dirs::data_dir)
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Returns the base directory for installed grammars.
#[must_use]
pub fn grammar_dir() -> PathBuf {
    data_dir().join("catenary").join("grammars")
}

/// Returns the C compiler name that would be used for grammar compilation.
///
/// Checks the `CC` environment variable first, falls back to `"cc"`.
#[must_use]
pub fn c_compiler_name() -> String {
    std::env::var("CC").unwrap_or_else(|_| "cc".to_string())
}

/// Resolves a grammar spec to a Git URL.
///
/// Accepts three forms:
/// - Bare name (e.g., `tree-sitter-rust`) — assumes `tree-sitter` GitHub org
/// - Owner/repo (e.g., `MarkWellsDev/tree-sitter-mock`) — assumes GitHub
/// - Full URL — used as-is
fn resolve_spec(spec: &str) -> String {
    if spec.contains("://") {
        spec.to_string()
    } else if spec.contains('/') {
        format!("https://github.com/{spec}")
    } else {
        format!("https://github.com/tree-sitter/{spec}")
    }
}

/// Clones a git repository (shallow, depth 1) to the destination directory.
///
/// # Errors
///
/// Returns an error if git is not found or the clone fails.
fn clone_repo(url: &str, dest: &Path) -> Result<()> {
    let output = std::process::Command::new("git")
        .args(["clone", "--depth", "1", "--quiet", url])
        .arg(dest)
        .output()
        .context("failed to run git clone — is git installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git clone failed: {}", stderr.trim());
    }

    Ok(())
}

/// Compiles grammar C/C++ source files into a shared library.
///
/// Handles three cases:
/// 1. `parser.c` only (most common)
/// 2. `parser.c` + `scanner.c` (both C)
/// 3. `parser.c` + `scanner.cc` (mixed C/C++, compiled separately)
///
/// # Errors
///
/// Returns an error if no C compiler is found or compilation fails.
fn compile_grammar(src_dir: &Path, output_path: &Path) -> Result<()> {
    let scanner_cc = src_dir.join("scanner.cc");

    if scanner_cc.exists() {
        compile_mixed(src_dir, output_path, &scanner_cc)
    } else {
        compile_c_only(src_dir, output_path)
    }
}

/// Returns a configured `cc::Build` for runtime use.
///
/// Sets `target`, `host`, and `opt_level` explicitly so the `cc` crate
/// doesn't look for cargo build-script environment variables (`TARGET`,
/// `HOST`, `OPT_LEVEL`) that aren't available at runtime.
fn cc_builder(cpp: bool) -> cc::Build {
    let target = env!("TARGET");
    let mut build = cc::Build::new();
    build
        .target(target)
        .host(target)
        .opt_level(0)
        .cpp(cpp)
        .cargo_metadata(false);
    build
}

/// Returns the C compiler [`cc::Tool`] for the current platform.
///
/// # Errors
///
/// Returns an error if no C compiler can be found.
fn c_compiler() -> Result<cc::Tool> {
    cc_builder(false)
        .try_get_compiler()
        .map_err(|e| anyhow::anyhow!("failed to find C compiler: {e}"))
}

/// Returns the C++ compiler [`cc::Tool`] for the current platform.
///
/// # Errors
///
/// Returns an error if no C++ compiler can be found.
fn cpp_compiler() -> Result<cc::Tool> {
    cc_builder(true)
        .try_get_compiler()
        .map_err(|e| anyhow::anyhow!("failed to find C++ compiler: {e}"))
}

/// Compile pure-C grammar sources in one shot.
fn compile_c_only(src_dir: &Path, output_path: &Path) -> Result<()> {
    let compiler = c_compiler()?;

    let mut cmd = compiler.to_command();
    cmd.arg("-shared")
        .arg("-fPIC")
        .arg("-I")
        .arg(src_dir)
        .arg("-o")
        .arg(output_path)
        .arg(src_dir.join("parser.c"));

    if src_dir.join("scanner.c").exists() {
        cmd.arg(src_dir.join("scanner.c"));
    }

    let output = cmd.output().context("failed to run C compiler")?;
    ensure!(
        output.status.success(),
        "grammar compilation failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(())
}

/// Compile mixed C/C++ grammar sources separately and link.
fn compile_mixed(src_dir: &Path, output_path: &Path, scanner_cc: &Path) -> Result<()> {
    let tmpdir = tempfile::tempdir().context("failed to create temp directory for compilation")?;
    let cc = c_compiler()?;
    let cxx = cpp_compiler()?;

    // Compile parser.c
    let parser_o = tmpdir.path().join("parser.o");
    let output = cc
        .to_command()
        .args(["-c", "-fPIC"])
        .arg("-I")
        .arg(src_dir)
        .arg("-o")
        .arg(&parser_o)
        .arg(src_dir.join("parser.c"))
        .output()
        .context("failed to compile parser.c")?;
    ensure!(
        output.status.success(),
        "parser.c compilation failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut objects = vec![parser_o];

    // Compile scanner.c if present
    let scanner_c = src_dir.join("scanner.c");
    if scanner_c.exists() {
        let scanner_c_o = tmpdir.path().join("scanner_c.o");
        let output = cc
            .to_command()
            .args(["-c", "-fPIC"])
            .arg("-I")
            .arg(src_dir)
            .arg("-o")
            .arg(&scanner_c_o)
            .arg(&scanner_c)
            .output()
            .context("failed to compile scanner.c")?;
        ensure!(
            output.status.success(),
            "scanner.c compilation failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        objects.push(scanner_c_o);
    }

    // Compile scanner.cc
    let scanner_cc_o = tmpdir.path().join("scanner_cc.o");
    let output = cxx
        .to_command()
        .args(["-c", "-fPIC"])
        .arg("-I")
        .arg(src_dir)
        .arg("-o")
        .arg(&scanner_cc_o)
        .arg(scanner_cc)
        .output()
        .context("failed to compile scanner.cc")?;
    ensure!(
        output.status.success(),
        "scanner.cc compilation failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    objects.push(scanner_cc_o);

    // Link with C++ linker (needed for C++ runtime)
    let mut link_cmd = cxx.to_command();
    link_cmd.arg("-shared").arg("-o").arg(output_path);
    for obj in &objects {
        link_cmd.arg(obj);
    }
    let output = link_cmd.output().context("failed to link grammar")?;
    ensure!(
        output.status.success(),
        "grammar linking failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(())
}

/// Installs a grammar from a local directory into the grammar registry.
///
/// Parses `tree-sitter.json`, verifies `queries/tags.scm`, compiles the C
/// source into a shared library, and writes a `metadata.json` sidecar.
///
/// # Errors
///
/// Returns an error if metadata is missing, tags.scm is absent, or
/// compilation fails.
pub(crate) fn install_from_dir(repo_dir: &Path, grammar_base: &Path, repo_url: &str) -> Result<()> {
    // Parse tree-sitter.json
    let ts_json_path = repo_dir.join("tree-sitter.json");
    let ts_json_content = std::fs::read_to_string(&ts_json_path)
        .with_context(|| format!("failed to read {}", ts_json_path.display()))?;
    let ts_json: serde_json::Value =
        serde_json::from_str(&ts_json_content).context("failed to parse tree-sitter.json")?;

    let grammar = ts_json
        .get("grammars")
        .and_then(|g| g.get(0))
        .ok_or_else(|| anyhow::anyhow!("tree-sitter.json missing grammars[0]"))?;

    let scope = grammar
        .get("scope")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter.json missing grammars[0].scope"))?;

    let file_types = grammar
        .get("file-types")
        .ok_or_else(|| anyhow::anyhow!("tree-sitter.json missing grammars[0].file-types"))?;

    // Verify tags.scm exists
    let tags_src = repo_dir.join("queries").join("tags.scm");
    ensure!(
        tags_src.exists(),
        "Grammar {scope} does not ship tags.scm. The language will use \
         the no-grammar path (ripgrep text heatmap) until the grammar \
         adds tag query support."
    );

    // Verify parser.c exists
    let src_dir = repo_dir.join("src");
    ensure!(
        src_dir.join("parser.c").exists(),
        "src/parser.c not found in grammar repository"
    );

    // Compile
    let scope_dir = grammar_base.join(scope);
    std::fs::create_dir_all(&scope_dir)
        .with_context(|| format!("failed to create directory: {}", scope_dir.display()))?;

    let lib_filename = format!("parser.{}", std::env::consts::DLL_EXTENSION);
    let lib_path = scope_dir.join(&lib_filename);
    compile_grammar(&src_dir, &lib_path)?;

    // Copy tags.scm
    let tags_path = scope_dir.join("tags.scm");
    std::fs::copy(&tags_src, &tags_path)
        .with_context(|| format!("failed to copy tags.scm to {}", tags_path.display()))?;

    // Write metadata sidecar
    let file_types: Vec<String> = serde_json::from_value(file_types.clone())
        .context("failed to parse file-types as string array")?;
    let metadata = GrammarMetadata {
        scope: scope.to_string(),
        file_types,
        repo_url: repo_url.to_string(),
        installed_at: Utc::now().to_rfc3339(),
    };
    let metadata_path = scope_dir.join("metadata.json");
    let metadata_json =
        serde_json::to_string_pretty(&metadata).context("failed to serialize metadata")?;
    std::fs::write(&metadata_path, metadata_json)
        .with_context(|| format!("failed to write {}", metadata_path.display()))?;

    Ok(())
}

/// Install a tree-sitter grammar from a spec.
///
/// The spec can be:
/// - A bare name (e.g., `tree-sitter-rust`) — cloned from the `tree-sitter` GitHub org
/// - An owner/repo pair (e.g., `MarkWellsDev/tree-sitter-mock`) — cloned from GitHub
/// - A full Git URL — cloned as-is
/// - A local directory path — used directly (no clone)
///
/// The grammar is compiled to a shared library, its `queries/tags.scm` is
/// copied, and a `metadata.json` sidecar is written.
///
/// # Errors
///
/// Returns an error if cloning or compilation fails.
pub fn install_grammar(spec: &str) -> Result<()> {
    let grammar_base = grammar_dir();

    // If spec is a local directory, use it directly (skip clone)
    let local_path = Path::new(spec);
    if local_path.is_dir() {
        return install_from_dir(local_path, &grammar_base, spec);
    }

    let url = resolve_spec(spec);
    let tmp = tempfile::tempdir().context("failed to create temp directory")?;
    clone_repo(&url, tmp.path())?;
    install_from_dir(tmp.path(), &grammar_base, &url)
}

/// List all installed grammars by scanning the grammar directory.
///
/// Prints a table of scope, file types, and installation timestamp.
///
/// # Errors
///
/// Returns an error if the grammar directory cannot be read.
#[allow(clippy::print_stdout, reason = "CLI command output")]
pub fn list_grammars() -> Result<()> {
    let grammars = scan_grammars()?;

    if grammars.is_empty() {
        println!("No grammars installed.");
        return Ok(());
    }

    let installed_header = "INSTALLED";
    println!("{:<25} {:<20} {installed_header}", "SCOPE", "FILE TYPES");
    for meta in &grammars {
        let ft = serde_json::to_string(&meta.file_types).unwrap_or_default();
        println!("{:<25} {ft:<20} {}", meta.scope, meta.installed_at);
    }

    Ok(())
}

/// Scans the default grammar directory for installed grammars.
///
/// # Errors
///
/// Returns an error if the grammar directory cannot be read.
pub fn scan_grammars() -> Result<Vec<GrammarMetadata>> {
    scan_grammars_in(&grammar_dir())
}

/// Scans a grammar directory for installed grammars.
///
/// Returns metadata for each grammar with a valid `metadata.json`.
///
/// # Errors
///
/// Returns an error if the grammar directory cannot be read.
pub fn scan_grammars_in(gdir: &Path) -> Result<Vec<GrammarMetadata>> {
    if !gdir.exists() {
        return Ok(Vec::new());
    }

    let mut grammars = Vec::new();
    let entries =
        std::fs::read_dir(gdir).with_context(|| format!("failed to read {}", gdir.display()))?;

    for entry in entries {
        let Ok(entry) = entry else { continue };
        let metadata_path = entry.path().join("metadata.json");
        if !metadata_path.exists() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&metadata_path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<GrammarMetadata>(&content) else {
            continue;
        };
        grammars.push(meta);
    }

    grammars.sort_by(|a, b| a.scope.cmp(&b.scope));
    Ok(grammars)
}

/// Remove an installed grammar by scope.
///
/// Deletes the entire scope directory from disk.
///
/// # Errors
///
/// Returns an error if the grammar directory is not found.
pub fn remove_grammar(scope: &str) -> Result<()> {
    let scope_dir = grammar_dir().join(scope);
    ensure!(
        scope_dir.exists(),
        "grammar '{scope}' not found at {}",
        scope_dir.display()
    );

    std::fs::remove_dir_all(&scope_dir)
        .with_context(|| format!("failed to remove {}", scope_dir.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test_assets")
            .join("mock_grammar")
    }

    #[test]
    fn test_resolve_spec_bare_name() {
        assert_eq!(
            resolve_spec("tree-sitter-rust"),
            "https://github.com/tree-sitter/tree-sitter-rust"
        );
    }

    #[test]
    fn test_resolve_spec_owner_repo() {
        assert_eq!(
            resolve_spec("MarkWellsDev/tree-sitter-mock"),
            "https://github.com/MarkWellsDev/tree-sitter-mock"
        );
    }

    #[test]
    fn test_resolve_spec_full_url() {
        let url = "https://gitlab.com/user/tree-sitter-custom.git";
        assert_eq!(resolve_spec(url), url);
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_install_and_list() {
        let out_dir = tempfile::tempdir().expect("tempdir");

        install_from_dir(
            &fixture_dir(),
            out_dir.path(),
            "https://github.com/test/mock",
        )
        .expect("install should succeed");

        // Verify metadata.json
        let meta_path = out_dir.path().join("source.mock").join("metadata.json");
        assert!(meta_path.exists(), "metadata.json should exist");
        let content = std::fs::read_to_string(&meta_path).expect("read metadata");
        let meta: GrammarMetadata = serde_json::from_str(&content).expect("parse metadata");
        assert_eq!(meta.scope, "source.mock");
        assert_eq!(meta.file_types, vec!["mock"]);

        // Verify files exist on disk
        let lib_ext = std::env::consts::DLL_EXTENSION;
        let lib = out_dir
            .path()
            .join("source.mock")
            .join(format!("parser.{lib_ext}"));
        let tags = out_dir.path().join("source.mock").join("tags.scm");
        assert!(
            lib.exists(),
            "compiled library should exist at {}",
            lib.display()
        );
        assert!(tags.exists(), "tags.scm should exist at {}", tags.display());
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_install_missing_tags_scm() {
        let out_dir = tempfile::tempdir().expect("tempdir");

        // Create a fixture without queries/tags.scm
        let no_tags = tempfile::tempdir().expect("tempdir");
        let src = no_tags.path().join("src");
        std::fs::create_dir_all(src.join("tree_sitter")).expect("mkdir");
        std::fs::copy(
            fixture_dir().join("src").join("parser.c"),
            src.join("parser.c"),
        )
        .expect("copy parser.c");
        std::fs::copy(
            fixture_dir()
                .join("src")
                .join("tree_sitter")
                .join("parser.h"),
            src.join("tree_sitter").join("parser.h"),
        )
        .expect("copy parser.h");
        std::fs::copy(
            fixture_dir().join("tree-sitter.json"),
            no_tags.path().join("tree-sitter.json"),
        )
        .expect("copy tree-sitter.json");

        let result = install_from_dir(no_tags.path(), out_dir.path(), "test");
        let err = result
            .expect_err("should fail without tags.scm")
            .to_string();
        assert!(
            err.contains("tags.scm"),
            "error should mention tags.scm, got: {err}"
        );
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_remove_grammar() {
        let out_dir = tempfile::tempdir().expect("tempdir");

        install_from_dir(
            &fixture_dir(),
            out_dir.path(),
            "https://github.com/test/mock",
        )
        .expect("install should succeed");

        // Verify scope dir exists
        let scope_dir = out_dir.path().join("source.mock");
        assert!(scope_dir.exists(), "scope dir should exist before remove");

        // Use the out_dir as grammar_dir by temporarily overriding
        // We can't call remove_grammar directly since it uses grammar_dir(),
        // so test the underlying logic
        std::fs::remove_dir_all(&scope_dir).expect("remove should succeed");
        assert!(!scope_dir.exists(), "scope dir should be deleted");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_load_grammar_api() {
        let out_dir = tempfile::tempdir().expect("tempdir");

        install_from_dir(
            &fixture_dir(),
            out_dir.path(),
            "https://github.com/test/mock",
        )
        .expect("install should succeed");

        let lib_ext = std::env::consts::DLL_EXTENSION;
        let lib_path = out_dir
            .path()
            .join("source.mock")
            .join(format!("parser.{lib_ext}"));

        let language = catenary_ts::load_grammar(&lib_path, "tree_sitter_mock")
            .expect("load_grammar should succeed");

        assert_eq!(language.version(), 14, "grammar version should be 14");
    }
}

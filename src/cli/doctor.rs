// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Doctor command: check language server health and hook configuration.

#![allow(clippy::print_stdout, reason = "CLI tool needs to output to stdout")]
#![allow(clippy::print_stderr, reason = "CLI tool needs to output to stderr")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use crate::cli::ColorConfig;
use crate::install;
use crate::lsp;

/// Expected Claude Code hooks, embedded at compile time.
const CLAUDE_HOOKS_EXPECTED: &str = include_str!("../../plugins/catenary/hooks/hooks.json");

/// Expected Gemini CLI hooks, embedded at compile time.
const GEMINI_HOOKS_EXPECTED: &str = include_str!("../../hooks/hooks.json");

/// Expected constrained-bash hook script, embedded at compile time.
const CONSTRAINED_BASH_EXPECTED: &str = include_str!("../../scripts/constrained_bash.py");

/// Run the doctor command: check language server health for the current workspace.
///
/// # Errors
///
/// Returns an error if the configuration cannot be loaded or roots cannot be resolved.
#[allow(
    clippy::too_many_lines,
    reason = "Doctor command has sequential output logic"
)]
pub async fn run_doctor(
    config_path: Option<&Path>,
    lsps: &[String],
    roots: &[PathBuf],
    nocolor: bool,
    show_diff: bool,
) -> Result<()> {
    let colors = ColorConfig::new(nocolor);

    // Print version header
    println!("Catenary {}", env!("CATENARY_VERSION"));
    println!();

    // Load configuration (same as run_server)
    let mut config = crate::config::Config::load(config_path.map(Path::to_path_buf))?;
    for lsp_spec in lsps {
        let (lang, command_str) = lsp_spec.split_once(':').ok_or_else(|| {
            anyhow::anyhow!("Invalid LSP spec: {lsp_spec}. Expected 'lang:command'")
        })?;
        let lang = lang.trim().to_string();
        let command_str = command_str.trim();
        let mut parts = command_str.split_whitespace();
        let program = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("command cannot be empty"))?
            .to_string();
        let cmd_args: Vec<String> = parts.map(std::string::ToString::to_string).collect();
        config.language.insert(
            lang,
            crate::config::LanguageConfig {
                command: Some(program),
                args: cmd_args,
                initialization_options: None,
                min_severity: None,
                settings: None,
                inherit: None,
            },
        );
    }

    // Resolve workspace roots
    let resolved_roots: Vec<PathBuf> = if roots.is_empty() {
        vec![PathBuf::from(".").canonicalize()?]
    } else {
        roots
            .iter()
            .map(|r| r.canonicalize())
            .collect::<std::io::Result<Vec<_>>>()?
    };

    // Print config and roots
    let config_source =
        config_path.map_or_else(|| "default paths".to_string(), |p| p.display().to_string());
    println!("{} {}", colors.bold("Config:"), config_source);
    println!(
        "{} {}",
        colors.bold("Roots: "),
        resolved_roots
            .iter()
            .map(|r| r.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!();

    // Deprecation warning
    if config.deprecated_server_key {
        println!(
            "{}",
            colors.yellow("⚠  Config uses deprecated [server.*] key — rename to [language.*]"),
        );
        println!();
    }

    if config.language.is_empty() {
        println!("No language servers configured.");
        return Ok(());
    }

    // Detect which languages have files in the workspace
    let configured_keys: std::collections::HashSet<&str> =
        config.language.keys().map(String::as_str).collect();
    let detected = lsp::detect_workspace_languages(&resolved_roots, &configured_keys);

    // Sort servers alphabetically — skip inherit-only entries without a command
    let mut servers: Vec<(&String, &crate::config::LanguageConfig)> = config
        .language
        .iter()
        .filter(|(_, lc)| lc.command.is_some())
        .collect();
    servers.sort_by_key(|(lang, _)| *lang);

    // Determine column width for language name
    let max_lang_width = servers.iter().map(|(l, _)| l.len()).max().unwrap_or(10);
    let max_cmd_width = servers
        .iter()
        .filter_map(|(_, s)| s.command.as_ref())
        .map(String::len)
        .max()
        .unwrap_or(10);

    for (lang, lang_config) in &servers {
        // command is guaranteed Some by the filter above
        let command = lang_config.command.as_deref().unwrap_or_default();
        let lang_display = format!("{lang:<max_lang_width$}");
        let cmd_display = format!("{command:<max_cmd_width$}");

        // Check if any files for this language exist
        if !detected.contains(lang.as_str()) {
            println!(
                "{}  {}  {}",
                colors.dim(&lang_display),
                colors.dim(&cmd_display),
                colors.dim("- skipped (no matching files)"),
            );
            continue;
        }

        // Check if binary exists on PATH
        if !binary_exists(command) {
            println!(
                "{}  {}  {}",
                lang_display,
                cmd_display,
                colors.red("✗ command not found"),
            );
            continue;
        }

        // Spawn and initialize the server
        let args_refs: Vec<&str> = lang_config.args.iter().map(String::as_str).collect();
        let spawn_result = lsp::LspClient::spawn_quiet(
            command,
            &args_refs,
            lang,
            Arc::new(crate::session::MessageLog::noop()),
        );

        let mut client = match spawn_result {
            Ok(client) => client,
            Err(e) => {
                println!(
                    "{}  {}  {}",
                    lang_display,
                    cmd_display,
                    colors.red(&format!("✗ spawn failed: {e}")),
                );
                continue;
            }
        };

        match client
            .initialize(&resolved_roots, lang_config.initialization_options.clone())
            .await
        {
            Ok(result) => {
                let tools =
                    extract_capabilities(&result["capabilities"], client.supports_type_hierarchy());
                println!(
                    "{}  {}  {}",
                    lang_display,
                    cmd_display,
                    colors.green("✓ ready"),
                );
                if !tools.is_empty() {
                    println!(
                        "{}  {}",
                        " ".repeat(max_lang_width + max_cmd_width + 4),
                        colors.dim(&tools.join(" ")),
                    );
                }
            }
            Err(e) => {
                println!(
                    "{}  {}  {}",
                    lang_display,
                    cmd_display,
                    colors.red(&format!("✗ initialize failed: {e}")),
                );
            }
        }

        // Shutdown cleanly
        let _ = client.shutdown().await;
    }

    // Hooks health section
    println!();
    println!("{}:", colors.bold("Hooks"));
    check_claude_hooks(&colors, show_diff);
    check_gemini_hooks(&colors, show_diff);
    check_path_binary(&colors);

    // Scripts health section
    println!();
    println!("{}:", colors.bold("Scripts"));
    check_constrained_bash_claude(&colors, show_diff);
    check_constrained_bash_gemini(&colors, show_diff);

    // Grammars health section
    println!();
    println!("{}:", colors.bold("Grammars"));
    check_grammars(&colors);

    Ok(())
}

/// Checks whether a binary can be found on `$PATH`.
fn binary_exists(command: &str) -> bool {
    // If the command contains a path separator, check it directly
    if command.contains('/') {
        return std::path::Path::new(command).exists();
    }

    // Search PATH
    let path_var = std::env::var("PATH").unwrap_or_default();
    std::env::split_paths(&path_var).any(|dir| dir.join(command).is_file())
}

/// Extracts Catenary tool names from LSP server capabilities.
fn extract_capabilities(caps: &serde_json::Value, type_hierarchy: bool) -> Vec<&'static str> {
    let has = |key: &str| caps.get(key).is_some_and(|v| !v.is_null());

    let mut tools = Vec::new();

    if has("hoverProvider") {
        tools.push("hover");
    }
    if has("definitionProvider") {
        tools.push("definition");
    }
    if has("typeDefinitionProvider") {
        tools.push("type_definition");
    }
    if has("implementationProvider") {
        tools.push("implementation");
    }
    if has("referencesProvider") {
        tools.push("references");
    }
    if has("documentSymbolProvider") {
        tools.push("document_symbols");
    }
    if has("workspaceSymbolProvider") {
        tools.push("search");
    }
    if has("codeActionProvider") {
        tools.push("code_actions");
    }
    if has("callHierarchyProvider") {
        tools.push("call_hierarchy");
    }
    if type_hierarchy {
        tools.push("type_hierarchy");
    }

    tools
}

/// Check Claude Code plugin hooks against the embedded expected hooks.
fn check_claude_hooks(colors: &ColorConfig, show_diff: bool) {
    let label = format!("{:<14}", "Claude Code");
    let Ok(home_str) = std::env::var("HOME") else {
        println!(
            "  {label}{}",
            colors.dim("- cannot determine home directory"),
        );
        return;
    };
    let home = PathBuf::from(home_str);

    let plugins_file = home.join(".claude/plugins/installed_plugins.json");
    let Ok(plugins_json) = std::fs::read_to_string(&plugins_file) else {
        println!("  {label}{}", colors.dim("- not installed"));
        return;
    };

    let Ok(plugins) = serde_json::from_str::<serde_json::Value>(&plugins_json) else {
        println!(
            "  {label}{}",
            colors.yellow("? cannot parse installed_plugins.json"),
        );
        return;
    };

    // Look up catenary@catenary in plugins.plugins
    let entries = match plugins
        .get("plugins")
        .and_then(|p| p.get("catenary@catenary"))
        .and_then(serde_json::Value::as_array)
    {
        Some(arr) if !arr.is_empty() => arr,
        _ => {
            println!("  {label}{}", colors.dim("- not installed"));
            return;
        }
    };

    // Use the first (most recent) entry
    let entry = &entries[0];
    let version = entry
        .get("version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let Some(install_path_str) = entry.get("installPath").and_then(serde_json::Value::as_str)
    else {
        println!(
            "  {label}{version:<8}{}",
            colors.yellow("? missing installPath"),
        );
        return;
    };
    let install_path = PathBuf::from(install_path_str);

    // Determine marketplace source type
    let source_type = read_marketplace_source(&home);
    let version_display = source_type
        .as_deref()
        .map_or_else(|| version.to_string(), |src| format!("{version} ({src})"));
    let ver_col = format!("{version_display:<20}");

    // Read installed hooks and compare
    let hooks_path = install_path.join("hooks/hooks.json");
    match std::fs::read_to_string(&hooks_path) {
        Ok(installed) => {
            if normalize_json(&installed) == normalize_json(CLAUDE_HOOKS_EXPECTED) {
                println!("  {label}{ver_col}{}", colors.green("✓ hooks match"));
            } else {
                println!(
                    "  {label}{ver_col}{}",
                    colors.red("✗ stale hooks (reinstall: claude plugin uninstall catenary@catenary && claude plugin install catenary@catenary)"),
                );
                if show_diff {
                    show_unified_diff(
                        &pretty_json(&installed),
                        &pretty_json(CLAUDE_HOOKS_EXPECTED),
                        "installed",
                        "expected",
                    );
                }
            }
        }
        Err(_) => {
            println!(
                "  {label}{ver_col}{}",
                colors.red("✗ hooks.json not found in plugin cache"),
            );
        }
    }
}

/// Read the catenary marketplace source type from `known_marketplaces.json`.
fn read_marketplace_source(home: &Path) -> Option<String> {
    let path = home.join(".claude/plugins/known_marketplaces.json");
    let contents = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&contents).ok()?;
    json.get("catenary")
        .and_then(|c| c.get("source"))
        .and_then(|s| s.get("source"))
        .and_then(serde_json::Value::as_str)
        .map(std::string::ToString::to_string)
}

/// Check Gemini CLI extension hooks against the embedded expected hooks.
fn check_gemini_hooks(colors: &ColorConfig, show_diff: bool) {
    let label = format!("{:<14}", "Gemini CLI");
    let Ok(home_str) = std::env::var("HOME") else {
        println!(
            "  {label}{}",
            colors.dim("- cannot determine home directory"),
        );
        return;
    };
    let home = PathBuf::from(home_str);

    // Look for the extension directory
    let ext_dir = home.join(".gemini/extensions");
    let candidates = ["Catenary", "catenary"];
    let ext_path = candidates
        .iter()
        .map(|name| ext_dir.join(name))
        .find(|p| p.is_dir());

    let Some(ext_path) = ext_path else {
        println!("  {label}{}", colors.dim("- not installed"));
        return;
    };

    // Read .gemini-extension-install.json to determine install type and source.
    // Gemini CLI writes this metadata file for both linked and installed extensions.
    let install_meta_path = ext_path.join(".gemini-extension-install.json");
    let install_meta = std::fs::read_to_string(&install_meta_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

    let install_type = install_meta
        .as_ref()
        .and_then(|m| m.get("type").and_then(serde_json::Value::as_str))
        .unwrap_or("unknown");

    // For linked extensions, the source field is a local path to the actual
    // extension files. For installed extensions (github-release, etc.), the
    // files are cloned into the extension directory itself.
    let resolved = if install_type == "link" {
        install_meta
            .as_ref()
            .and_then(|m| m.get("source").and_then(serde_json::Value::as_str))
            .map_or_else(|| ext_path.clone(), PathBuf::from)
    } else {
        ext_path
    };

    // Read the extension manifest for version info
    let manifest_path = resolved.join("gemini-extension.json");
    let version = std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("version")
                .and_then(serde_json::Value::as_str)
                .map(std::string::ToString::to_string)
        });

    let type_label = if install_type == "link" {
        "linked"
    } else {
        "installed"
    };
    let version_display = version
        .as_deref()
        .map_or_else(|| type_label.to_string(), |v| format!("{v} ({type_label})"));
    let ver_col = format!("{version_display:<20}");

    // Read hooks and compare against embedded
    let hooks_path = resolved.join("hooks/hooks.json");
    match std::fs::read_to_string(&hooks_path) {
        Ok(installed) => {
            if normalize_json(&installed) == normalize_json(GEMINI_HOOKS_EXPECTED) {
                println!("  {label}{ver_col}{}", colors.green("✓ hooks match"));
            } else {
                println!(
                    "  {label}{ver_col}{}",
                    colors.red("✗ stale hooks (update extension)"),
                );
                if show_diff {
                    show_unified_diff(
                        &pretty_json(&installed),
                        &pretty_json(GEMINI_HOOKS_EXPECTED),
                        "installed",
                        "expected",
                    );
                }
            }
        }
        Err(_) => {
            println!(
                "  {label}{ver_col}{}",
                colors.yellow("? hooks.json not found"),
            );
        }
    }
}

/// Check whether the running binary matches what `$PATH` would resolve.
fn check_path_binary(colors: &ColorConfig) {
    let label = format!("{:<14}", "PATH");
    let spacer = " ".repeat(20);

    let Some(current_exe) = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
    else {
        println!(
            "  {label}{}",
            colors.yellow("? cannot determine current executable"),
        );
        return;
    };

    // Find catenary on PATH
    let path_var = std::env::var("PATH").unwrap_or_default();
    let Some(path_binary) = std::env::split_paths(&path_var)
        .map(|dir| dir.join("catenary"))
        .find(|p| p.is_file())
    else {
        println!(
            "  {label}{spacer}{}",
            colors.red("✗ catenary not found on PATH"),
        );
        return;
    };

    let resolved_path = std::fs::canonicalize(&path_binary).unwrap_or(path_binary);

    if current_exe == resolved_path {
        println!(
            "  {label}{spacer}{}",
            colors.green(&format!("✓ {}", resolved_path.display())),
        );
    } else {
        println!(
            "  {label}{spacer}{}",
            colors.red(&format!(
                "✗ {} differs from {}",
                resolved_path.display(),
                current_exe.display(),
            )),
        );
    }
}

/// Check the constrained-bash script referenced in `~/.claude/settings.json`.
fn check_constrained_bash_claude(colors: &ColorConfig, show_diff: bool) {
    let label = format!("{:<14}", "Claude Code");

    let Ok(home_str) = std::env::var("HOME") else {
        println!(
            "  {label}{}",
            colors.dim("- cannot determine home directory")
        );
        return;
    };
    let home = PathBuf::from(home_str);

    let settings_path = home.join(".claude/settings.json");
    let Ok(settings_json) = std::fs::read_to_string(&settings_path) else {
        println!("  {label}{}", colors.dim("- not configured"));
        return;
    };

    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&settings_json) else {
        println!("  {label}{}", colors.yellow("? cannot parse settings.json"));
        return;
    };

    let Some(script_token) = find_script_path_in_json(&settings, "constrained_bash.py") else {
        println!("  {label}{}", colors.dim("- not configured"));
        return;
    };

    let script_path = expand_home(&script_token, &home);

    match std::fs::read_to_string(&script_path) {
        Ok(installed) => {
            if installed == CONSTRAINED_BASH_EXPECTED {
                println!("  {label}{}", colors.green("✓ up to date"));
            } else if show_diff {
                println!("  {label}{}", colors.red("✗ out of date"));
                show_unified_diff(
                    &installed,
                    CONSTRAINED_BASH_EXPECTED,
                    "installed",
                    "expected",
                );
            } else {
                println!(
                    "  {label}{}",
                    colors.red("✗ out of date (run catenary doctor --diff to see changes)"),
                );
            }
        }
        Err(_) => {
            println!(
                "  {label}{}",
                colors.red(&format!("✗ not found at {}", script_path.display())),
            );
        }
    }
}

/// Check the constrained-bash script referenced in `~/.gemini/settings.json`.
fn check_constrained_bash_gemini(colors: &ColorConfig, show_diff: bool) {
    let label = format!("{:<14}", "Gemini CLI");

    let Ok(home_str) = std::env::var("HOME") else {
        println!(
            "  {label}{}",
            colors.dim("- cannot determine home directory")
        );
        return;
    };
    let home = PathBuf::from(home_str);

    let settings_path = home.join(".gemini/settings.json");
    let Ok(settings_json) = std::fs::read_to_string(&settings_path) else {
        println!("  {label}{}", colors.dim("- not configured"));
        return;
    };

    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&settings_json) else {
        println!("  {label}{}", colors.yellow("? cannot parse settings.json"));
        return;
    };

    let Some(script_token) = find_script_path_in_json(&settings, "constrained_bash.py") else {
        println!("  {label}{}", colors.dim("- not configured"));
        return;
    };

    let script_path = expand_home(&script_token, &home);

    match std::fs::read_to_string(&script_path) {
        Ok(installed) => {
            if installed == CONSTRAINED_BASH_EXPECTED {
                println!("  {label}{}", colors.green("✓ up to date"));
            } else if show_diff {
                println!("  {label}{}", colors.red("✗ out of date"));
                show_unified_diff(
                    &installed,
                    CONSTRAINED_BASH_EXPECTED,
                    "installed",
                    "expected",
                );
            } else {
                println!(
                    "  {label}{}",
                    colors.red("✗ out of date (run catenary doctor --diff to see changes)"),
                );
            }
        }
        Err(_) => {
            println!(
                "  {label}{}",
                colors.red(&format!("✗ not found at {}", script_path.display())),
            );
        }
    }
}

/// Normalize a JSON string for comparison (parse and re-serialize).
///
/// Returns the compact re-serialized form, or the original string (trimmed)
/// if parsing fails.
fn normalize_json(s: &str) -> String {
    serde_json::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| serde_json::to_string(&v).ok())
        .unwrap_or_else(|| s.trim().to_string())
}

/// Pretty-print a JSON string for use in human-readable diffs.
///
/// Returns the pretty-printed form, or the original string if parsing fails.
fn pretty_json(s: &str) -> String {
    serde_json::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| s.to_string())
}

/// Print a unified diff between `old` and `new` using the `similar` crate.
fn show_unified_diff(old: &str, new: &str, old_label: &str, new_label: &str) {
    use similar::TextDiff;
    let diff = TextDiff::from_lines(old, new);
    print!(
        "{}",
        diff.unified_diff()
            .context_radius(3)
            .header(old_label, new_label)
    );
}

/// Walk all string values in `json` and return the whitespace-split token
/// that contains `needle`, searching depth-first.
///
/// Returns `None` if no string value in the tree mentions `needle`.
fn find_script_path_in_json(json: &serde_json::Value, needle: &str) -> Option<String> {
    match json {
        serde_json::Value::String(s) if s.contains(needle) => s
            .split_whitespace()
            .find(|token| token.contains(needle))
            .map(std::string::ToString::to_string),
        serde_json::Value::Object(map) => map
            .values()
            .find_map(|v| find_script_path_in_json(v, needle)),
        serde_json::Value::Array(arr) => {
            arr.iter().find_map(|v| find_script_path_in_json(v, needle))
        }
        _ => None,
    }
}

/// Expand `$HOME/` and `~/` prefixes in a path string.
fn expand_home(path_str: &str, home: &Path) -> PathBuf {
    path_str
        .strip_prefix("$HOME/")
        .or_else(|| path_str.strip_prefix("~/"))
        .map_or_else(
            || {
                if path_str == "$HOME" || path_str == "~" {
                    home.to_path_buf()
                } else {
                    PathBuf::from(path_str)
                }
            },
            |rest| home.join(rest),
        )
}

/// Check grammar toolchain and installed grammars.
fn check_grammars(colors: &ColorConfig) {
    check_grammars_compiler(colors);
    check_grammars_dir(colors);

    let Ok(db) = crate::db::open_and_migrate() else {
        println!("  {}", colors.red("✗ failed to open database"));
        return;
    };
    check_grammars_installed(colors, &db);
}

/// Check whether a C compiler is available for grammar compilation.
fn check_grammars_compiler(colors: &ColorConfig) {
    let cc_name = install::c_compiler_name();
    if binary_exists(&cc_name) {
        println!("  {}", colors.green(&format!("✓ {cc_name} found")));
    } else {
        println!(
            "  {}",
            colors.red("✗ C compiler not found — catenary install requires a C compiler"),
        );
    }
}

/// Print the grammar data directory path and whether it exists.
fn check_grammars_dir(colors: &ColorConfig) {
    let gdir = install::grammar_dir();
    if gdir.exists() {
        println!("  {}", gdir.display());
    } else {
        println!(
            "  {}",
            colors.dim(&format!("{} (not yet created)", gdir.display())),
        );
    }
}

/// List installed grammars and verify their files exist on disk.
pub(crate) fn check_grammars_installed(colors: &ColorConfig, db: &rusqlite::Connection) {
    let Ok(mut stmt) = db.prepare("SELECT scope, lib_path, tags_path FROM grammars ORDER BY scope")
    else {
        println!("  {}", colors.red("✗ failed to query grammars"));
        return;
    };

    let rows: Vec<(String, String, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .ok()
        .map(|iter| iter.filter_map(Result::ok).collect())
        .unwrap_or_default();

    if rows.is_empty() {
        println!("  {}", colors.dim("(none installed)"));
        return;
    }

    for (scope, lib_path, tags_path) in &rows {
        let lib_ok = Path::new(lib_path).exists();
        let tags_ok = Path::new(tags_path).exists();

        if lib_ok && tags_ok {
            println!("  {}", colors.green(&format!("✓ {scope}")));
        } else if !lib_ok {
            let lib_name = Path::new(lib_path)
                .file_name()
                .map_or("parser.so", |n| n.to_str().unwrap_or("parser.so"));
            println!(
                "  {}",
                colors.red(&format!("✗ {scope} — missing {lib_name}")),
            );
        } else {
            println!("  {}", colors.red(&format!("✗ {scope} — missing tags.scm")),);
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::cli::ColorConfig;

    /// Open an isolated test database in a tempdir.
    fn test_db() -> (tempfile::TempDir, std::path::PathBuf, rusqlite::Connection) {
        let dir = tempfile::tempdir().expect("failed to create tempdir for test DB");
        let path = dir.path().join("catenary").join("catenary.db");
        let conn = crate::db::open_and_migrate_at(&path).expect("failed to open test DB");
        (dir, path, conn)
    }

    #[test]
    fn test_doctor_grammar_section_no_grammars() {
        let (_dir, _path, conn) = test_db();
        let colors = ColorConfig::new(true);

        // Should not panic on empty grammars table
        check_grammars_installed(&colors, &conn);
    }

    #[test]
    fn test_doctor_grammar_section_with_grammar() {
        let (_dir, _path, conn) = test_db();
        let colors = ColorConfig::new(true);

        // Insert a grammar row with paths that don't exist on disk
        conn.execute(
            "INSERT INTO grammars (scope, file_types, lib_path, tags_path, repo_url, installed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                "source.mock",
                r#"["mock"]"#,
                "/nonexistent/parser.so",
                "/nonexistent/tags.scm",
                "https://github.com/test/mock",
                "2026-03-07T12:00:00Z",
            ],
        )
        .expect("insert grammar row");

        // Should not panic; will report missing files
        check_grammars_installed(&colors, &conn);
    }
}

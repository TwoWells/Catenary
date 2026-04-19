// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Doctor command: check language server health and hook configuration.

#![allow(clippy::print_stdout, reason = "CLI tool needs to output to stdout")]
#![allow(clippy::print_stderr, reason = "CLI tool needs to output to stderr")]

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::bridge::filesystem_manager::FilesystemManager;
use crate::cli::ColorConfig;
use crate::install;
use crate::lsp;

/// Expected Claude Code hooks, embedded at compile time.
const CLAUDE_HOOKS_EXPECTED: &str = include_str!("../../plugins/catenary/hooks/hooks.json");

/// Expected Gemini CLI hooks, embedded at compile time.
const GEMINI_HOOKS_EXPECTED: &str = include_str!("../../hooks/hooks.json");

/// Expected constrained-bash hook script, embedded at compile time.
const CONSTRAINED_BASH_EXPECTED: &str = include_str!("../../scripts/constrained_bash.py");

/// Run the doctor command: check language server health.
///
/// When `roots` is empty, tests all configured language servers. When
/// roots are provided, only tests servers for languages detected in
/// those directories.
///
/// # Errors
///
/// Returns an error if the configuration cannot be loaded or roots cannot be resolved.
#[allow(
    clippy::too_many_lines,
    reason = "Doctor command has sequential output logic"
)]
pub async fn run_doctor(roots: &[PathBuf], nocolor: bool, show_diff: bool) -> Result<()> {
    let colors = ColorConfig::new(nocolor);

    // Print version header
    println!("Catenary {}", env!("CATENARY_VERSION"));
    println!();

    // Check config sources for old-format entries before loading
    doctor_check_config(&colors);

    // Load configuration — report errors inline instead of bailing
    let config = match crate::config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            println!("{}", colors.red(&format!("✗ Config error: {e:#}")));
            println!();
            return Ok(());
        }
    };

    // Resolve workspace roots (if provided)
    let resolved_roots: Option<Vec<PathBuf>> = if roots.is_empty() {
        None
    } else {
        Some(
            roots
                .iter()
                .map(|r| r.canonicalize())
                .collect::<std::io::Result<Vec<_>>>()?,
        )
    };

    // Print config header
    let config_source = std::env::var("CATENARY_CONFIG")
        .ok()
        .unwrap_or_else(|| "default paths".to_string());
    println!("{} {}", colors.bold("Config:"), config_source);
    if let Some(ref resolved) = resolved_roots {
        println!(
            "{} {}",
            colors.bold("Roots: "),
            resolved
                .iter()
                .map(|r| r.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!();

    // Validation errors
    let validation_errors = config.validate();
    for err in &validation_errors {
        println!("{}", colors.red(&format!("✗  {err}")));
    }

    // Unreferenced server warnings
    let referenced: HashSet<&str> = config
        .language
        .values()
        .flat_map(|lc| lc.servers.iter().map(|b| b.name.as_str()))
        .collect();
    let mut unreferenced: Vec<&str> = config
        .server
        .keys()
        .filter(|name| !referenced.contains(name.as_str()))
        .map(String::as_str)
        .collect();
    unreferenced.sort_unstable();
    for name in &unreferenced {
        println!(
            "{}",
            colors.yellow(&format!(
                "⚠  Server '{name}' is defined but not referenced by any [language.*] entry"
            )),
        );
    }

    if !validation_errors.is_empty() || !unreferenced.is_empty() {
        println!();
    }

    if config.language.is_empty() && config.server.is_empty() {
        println!("No language servers configured.");
        return Ok(());
    }

    // Detect which languages have files in the workspace (only when roots provided)
    let detected: Option<HashSet<String>> = resolved_roots.as_ref().map(|roots| {
        let configured_keys: HashSet<&str> = config.language.keys().map(String::as_str).collect();
        let fs = FilesystemManager::new();
        fs.detect_workspace_languages(roots, &configured_keys)
    });

    // ── Servers section ──────────────────────────────────────────────
    // Spawn each unique server once, collect capabilities.
    let mut server_names: Vec<&str> = config.server.keys().map(String::as_str).collect();
    server_names.sort_unstable();

    let max_server_width = server_names.iter().map(|n| n.len()).max().unwrap_or(10);

    println!("{}:", colors.bold("Servers"));
    let mut server_capabilities: std::collections::HashMap<&str, Vec<&'static str>> =
        std::collections::HashMap::new();
    for name in &server_names {
        let server_def = &config.server[*name];
        let command = server_def.command.as_str();
        let name_display = format!("  {name:<max_server_width$}");

        if !binary_exists(command) {
            println!(
                "{name_display}  {}",
                colors.red(&format!("✗ {command}: command not found")),
            );
            continue;
        }

        let args_refs: Vec<&str> = server_def.args.iter().map(String::as_str).collect();
        let spawn_result = lsp::LspClient::spawn_quiet(
            command,
            &args_refs,
            name,
            crate::logging::LoggingServer::new(),
        );

        let mut client = match spawn_result {
            Ok(client) => client,
            Err(e) => {
                println!(
                    "{name_display}  {}",
                    colors.red(&format!("✗ spawn failed: {e}")),
                );
                continue;
            }
        };

        let init_roots = resolved_roots.as_deref().unwrap_or(&[]);
        match client
            .initialize(init_roots, server_def.initialization_options.clone())
            .await
        {
            Ok(result) => {
                let tools =
                    extract_capabilities(&result["capabilities"], client.supports_type_hierarchy());
                println!("{name_display}  {}", colors.green("✓ ready"));
                server_capabilities.insert(name, tools);
            }
            Err(e) => {
                println!(
                    "{name_display}  {}",
                    colors.red(&format!("✗ initialize failed: {e}")),
                );
            }
        }

        let _ = client.shutdown().await;
    }

    // ── Languages section ────────────────────────────────────────────
    println!();
    println!("{}:", colors.bold("Languages"));

    // Build sorted list of (language, server_name) pairs
    let mut lang_entries: Vec<(&str, &str)> = Vec::new();
    for (lang, lc) in &config.language {
        if let Some(binding) = lc.servers.first() {
            lang_entries.push((lang.as_str(), binding.name.as_str()));
        }
    }
    lang_entries.sort_by_key(|(lang, _)| *lang);

    let max_lang_width = lang_entries
        .iter()
        .map(|(l, _)| l.len())
        .max()
        .unwrap_or(10);

    for (lang, target) in &lang_entries {
        let lang_display = format!("  {lang:<max_lang_width$}");

        // Check if any files for this language exist (only when roots provided)
        if let Some(ref det) = detected
            && !det.contains(*lang)
        {
            println!(
                "{}  {}",
                colors.dim(&lang_display),
                colors.dim(&format!("→ {target}  - skipped (no matching files)")),
            );
            continue;
        }

        println!("{lang_display}  → {target}");
        // Show capabilities from the server, indented
        if let Some(tools) = server_capabilities.get(target)
            && !tools.is_empty()
        {
            println!(
                "{}    {}",
                " ".repeat(max_lang_width + 2),
                colors.dim(&tools.join(" ")),
            );
        }
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

/// Check config source files for old-format entries and print migration guidance.
///
/// Reads each config file as raw TOML (independent of `Config::load`) to detect:
/// - `[server.*]` with no `[language.*]` (old deprecated format)
/// - `[language.*]` entries containing `command`/`args` etc. (intermediate format)
///
/// Prints the equivalent new-format config for each detected old entry.
fn doctor_check_config(colors: &ColorConfig) {
    let sources = crate::config::config_sources();
    let mut found_issues = false;

    for source in &sources {
        let Ok(contents) = std::fs::read_to_string(source) else {
            continue;
        };
        let Ok(raw) = contents.parse::<toml::Value>() else {
            continue;
        };

        let has_server = raw.get("server").is_some();
        let has_language = raw.get("language").is_some();

        // Old deprecated format: [server.*] with command fields and no [language.*]
        if has_server
            && !has_language
            && let Some(table) = raw.get("server").and_then(toml::Value::as_table)
        {
            for (key, entry) in table {
                if let Some(entry_table) = entry.as_table()
                    && entry_table.contains_key("command")
                {
                    found_issues = true;
                    print_migration(colors, source, key, entry_table, true);
                }
            }
        }

        // [language.*] entries with removed or stale fields
        if let Some(table) = raw.get("language").and_then(toml::Value::as_table) {
            for (key, entry) in table {
                if let Some(entry_table) = entry.as_table() {
                    // Removed field: inherit
                    if entry_table.contains_key("inherit") {
                        found_issues = true;
                        let target = entry_table
                            .get("inherit")
                            .and_then(toml::Value::as_str)
                            .unwrap_or("?");
                        println!(
                            "{}",
                            colors.yellow(&format!(
                                "⚠  {}: [language.{key}] uses removed `inherit` field — \
                                 copy `servers` list from [language.{target}] into \
                                 [language.{key}] instead.",
                                source.display(),
                            )),
                        );
                    }

                    // Intermediate format: inline server definition fields
                    let has_server_fields = crate::config::SERVER_DEF_KEYS
                        .iter()
                        .any(|k| entry_table.contains_key(*k));
                    if has_server_fields {
                        found_issues = true;
                        print_migration(colors, source, key, entry_table, false);
                    }
                }
            }
        }
    }

    if found_issues {
        println!();
    }
}

/// Print migration guidance for a single old-format entry.
fn print_migration(
    colors: &ColorConfig,
    source: &Path,
    key: &str,
    entry: &toml::map::Map<String, toml::Value>,
    is_server_section: bool,
) {
    let section = if is_server_section {
        "server"
    } else {
        "language"
    };
    println!(
        "{}",
        colors.yellow(&format!(
            "⚠  {}: [{section}.{key}] uses old format — migrate to [language.*] + [server.*]:",
            source.display(),
        )),
    );

    // Determine server name from command, falling back to the key
    let server_name = entry
        .get("command")
        .and_then(toml::Value::as_str)
        .unwrap_or(key);

    // Build old-format display
    println!();
    println!("  Old:");
    println!("    [{section}.{key}]");
    for (k, v) in entry {
        println!("    {k} = {v}");
    }

    // Build new-format display
    let server_fields: Vec<(&str, &toml::Value)> = crate::config::SERVER_DEF_KEYS
        .iter()
        .filter_map(|k| entry.get(*k).map(|v| (*k, v)))
        .collect();
    let lang_fields: Vec<(&str, &toml::Value)> = entry
        .iter()
        .filter(|(k, _)| !crate::config::SERVER_DEF_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.as_str(), v))
        .collect();

    println!();
    println!("  New:");
    println!("    [language.{key}]");
    println!("    servers = [\"{server_name}\"]");
    for (k, v) in &lang_fields {
        println!("    {k} = {v}");
    }
    println!();
    println!("    [server.{server_name}]");
    for (k, v) in &server_fields {
        println!("    {k} = {v}");
    }
    println!();
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

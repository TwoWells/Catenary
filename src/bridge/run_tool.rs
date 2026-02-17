/*
 * Copyright (C) 2026 Mark Wells Dev
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

//! Shell execution tool with allowlist enforcement.
//!
//! The `run` tool executes shell commands with security controls:
//! - Commands must be on an explicit allowlist (`allowed = ["*"]` opts in to unrestricted).
//! - Language-specific commands activate when matching files exist in the workspace.
//! - The tool description dynamically reflects the current allowlist.
//! - Commands are executed directly (not via shell) to prevent injection.

use anyhow::{Result, anyhow};
use ignore::WalkBuilder;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use tracing::debug;

use crate::config::RunToolConfig;
use crate::mcp::CallToolResult;

use super::handler::LspBridgeHandler;

/// Maximum output size per stream (stdout/stderr) in bytes.
const MAX_OUTPUT_BYTES: usize = 100 * 1024; // 100KB

/// Default command timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Maximum depth for language detection file scan.
const LANGUAGE_SCAN_DEPTH: usize = 2;

/// Input for the `run` tool.
#[derive(Debug, Deserialize)]
pub struct RunInput {
    /// The command to execute. Accepts a binary name (e.g., `"cargo"`) or a
    /// single string with arguments (e.g., `"cargo test --lib"`).
    pub command: String,
    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Timeout in seconds (default: 120). Accepts integer or string.
    pub timeout: Option<serde_json::Value>,
    /// Working directory for the command. Must be within workspace roots.
    pub cwd: Option<String>,
    /// Content to pipe to the process's standard input.
    pub stdin: Option<String>,
    /// Capture stdout to a file instead of returning it inline.
    /// Path must be within workspace roots.
    pub output_file: Option<String>,
    /// Seconds to sleep before executing the command. Must be paired with
    /// a command — standalone sleep is not permitted.
    pub sleep: Option<f64>,
}

/// Output from a command execution.
pub struct RunOutput {
    /// Standard output (may be truncated).
    pub stdout: String,
    /// Standard error (may be truncated).
    pub stderr: String,
    /// Exit code, or `None` if the process was killed.
    pub exit_code: Option<i32>,
    /// Whether the command was killed due to timeout.
    pub timed_out: bool,
}

/// Manages shell command execution with allowlist enforcement.
pub struct RunToolManager {
    /// Static allowlist from config.
    base_allowed: Vec<String>,
    /// Denied command+subcommand pairs parsed from config.
    /// Each entry is `(command, subcommand)`.
    denied_subcommands: Vec<(String, String)>,
    /// Language-specific commands from config.
    language_configs: HashMap<String, Vec<String>>,
    /// Currently detected languages in workspace.
    detected_languages: HashSet<String>,
    /// Workspace roots for CWD.
    roots: Vec<PathBuf>,
    /// Whether unrestricted mode is enabled (`allowed = ["*"]`).
    unrestricted: bool,
}

impl RunToolManager {
    /// Creates a new `RunToolManager` from config and workspace roots.
    #[must_use]
    pub fn new(config: &RunToolConfig, roots: &[PathBuf]) -> Self {
        let unrestricted = config.allowed.iter().any(|a| a == "*");

        let language_configs: HashMap<String, Vec<String>> = config
            .languages
            .iter()
            .map(|(k, v)| (k.clone(), v.allowed.clone()))
            .collect();

        let base_allowed: Vec<String> = config
            .allowed
            .iter()
            .filter(|a| a.as_str() != "*")
            .cloned()
            .collect();

        let denied_subcommands: Vec<(String, String)> = config
            .denied
            .iter()
            .filter_map(|entry| {
                let mut parts = entry.splitn(2, ' ');
                let cmd = parts.next()?.to_string();
                let sub = parts.next()?.to_string();
                Some((cmd, sub))
            })
            .collect();

        let mut manager = Self {
            base_allowed,
            denied_subcommands,
            language_configs,
            detected_languages: HashSet::new(),
            roots: roots.to_vec(),
            unrestricted,
        };

        manager.detect_languages(roots);
        manager
    }

    /// Scans workspace roots for file extensions and detects languages.
    /// Returns `true` if the set of detected languages changed.
    pub fn detect_languages(&mut self, roots: &[PathBuf]) -> bool {
        let mut detected = HashSet::new();

        for root in roots {
            if !root.exists() {
                continue;
            }

            let walker = WalkBuilder::new(root)
                .max_depth(Some(LANGUAGE_SCAN_DEPTH))
                .git_ignore(true)
                .hidden(true)
                .build();

            for entry in walker.flatten() {
                if let Some(ext) = entry.path().extension().and_then(|e| e.to_str())
                    && let Some(lang) = extension_to_language(ext)
                    && self.language_configs.contains_key(lang)
                {
                    detected.insert(lang.to_string());
                }
            }
        }

        let changed = detected != self.detected_languages;
        if changed {
            debug!(
                "Language detection: {:?} -> {:?}",
                self.detected_languages, detected
            );
        }
        self.detected_languages = detected;
        changed
    }

    /// Validates that a command is on the allowlist and not denied.
    ///
    /// # Errors
    ///
    /// Returns an error if the command+subcommand is denied, or if the command
    /// is not on the allowlist.
    pub fn validate_command(&self, command: &str, args: &[String]) -> Result<()> {
        // Denied subcommands take priority over everything, including unrestricted
        if let Some(first_arg) = args.first() {
            for (denied_cmd, denied_sub) in &self.denied_subcommands {
                if denied_cmd == command && denied_sub == first_arg {
                    return Err(anyhow!(
                        "Command '{command} {first_arg}' is denied. {}",
                        self.describe_allowlist()
                    ));
                }
            }
        }

        if self.unrestricted {
            return Ok(());
        }

        // Check base allowlist
        if self.base_allowed.iter().any(|a| a == command) {
            return Ok(());
        }

        // Check language-specific allowlists for detected languages
        for lang in &self.detected_languages {
            if let Some(commands) = self.language_configs.get(lang)
                && commands.iter().any(|c| c == command)
            {
                return Ok(());
            }
        }

        Err(anyhow!(
            "Command '{}' is not allowed. {}",
            command,
            self.describe_allowlist()
        ))
    }

    /// Returns a human-readable description of the current allowlist.
    #[must_use]
    pub fn describe_allowlist(&self) -> String {
        let mut parts = Vec::new();

        if self.unrestricted {
            parts.push("All commands are allowed".to_string());
        } else {
            if !self.base_allowed.is_empty() {
                parts.push(format!("Allowed: {}", self.base_allowed.join(", ")));
            }

            for lang in &self.detected_languages {
                if let Some(commands) = self.language_configs.get(lang) {
                    parts.push(format!("{lang} (detected): {}", commands.join(", ")));
                }
            }

            // Include configured but not-detected languages for completeness
            for (lang, commands) in &self.language_configs {
                if !self.detected_languages.contains(lang) {
                    parts.push(format!("{lang} (not detected): {}", commands.join(", ")));
                }
            }
        }

        if !self.denied_subcommands.is_empty() {
            let denied: Vec<String> = self
                .denied_subcommands
                .iter()
                .map(|(cmd, sub)| format!("{cmd} {sub}"))
                .collect();
            parts.push(format!("Denied: {}", denied.join(", ")));
        }

        if parts.is_empty() {
            "No commands are allowed.".to_string()
        } else {
            parts.join(". ") + "."
        }
    }

    /// Executes a command with timeout and output limits.
    ///
    /// # Errors
    ///
    /// Returns an error if the command cannot be spawned.
    pub async fn execute(
        &self,
        command: &str,
        args: &[String],
        timeout_secs: Option<u64>,
        cwd: Option<&Path>,
        stdin_content: Option<&str>,
    ) -> Result<RunOutput> {
        let timeout = std::time::Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));

        let cwd = cwd.map_or_else(
            || {
                self.roots.first().cloned().unwrap_or_else(|| {
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                })
            },
            Path::to_path_buf,
        );

        debug!("Executing: {} {:?} in {:?}", command, args, cwd);

        let stdin_cfg = if stdin_content.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        };

        let mut child = tokio::process::Command::new(command)
            .args(args)
            .current_dir(&cwd)
            .stdin(stdin_cfg)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("Failed to spawn command '{command}': {e}"))?;

        if let Some(content) = stdin_content
            && let Some(mut pipe) = child.stdin.take()
        {
            use tokio::io::AsyncWriteExt;
            pipe.write_all(content.as_bytes())
                .await
                .map_err(|e| anyhow!("Failed to write to stdin: {e}"))?;
            // Drop the pipe to close stdin
        }

        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => {
                let stdout = truncate_output(&output.stdout);
                let stderr = truncate_output(&output.stderr);

                Ok(RunOutput {
                    stdout,
                    stderr,
                    exit_code: output.status.code(),
                    timed_out: false,
                })
            }
            Ok(Err(e)) => Err(anyhow!("Command failed: {e}")),
            Err(_) => {
                // Timeout — the child process is dropped, which kills it
                Ok(RunOutput {
                    stdout: String::new(),
                    stderr: format!("Command timed out after {}s", timeout.as_secs()),
                    exit_code: None,
                    timed_out: true,
                })
            }
        }
    }

    /// Updates workspace roots and re-detects languages.
    /// Returns `true` if the allowlist changed (detected languages differ).
    pub fn update_roots(&mut self, roots: &[PathBuf]) -> bool {
        self.roots = roots.to_vec();
        self.detect_languages(roots)
    }

    /// Checks if a file path introduces a new language to the detected set.
    ///
    /// If the file's extension maps to a configured language that hasn't been
    /// detected yet, marks it as detected and returns `true`. This is an O(1)
    /// check — no filesystem scan is performed.
    pub fn maybe_detect_language(&mut self, path: &Path) -> bool {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return false;
        };
        let Some(lang) = extension_to_language(ext) else {
            return false;
        };
        if !self.language_configs.contains_key(lang) {
            return false;
        }
        if self.detected_languages.contains(lang) {
            return false;
        }
        debug!("New language detected from file write: {lang}");
        self.detected_languages.insert(lang.to_string());
        true
    }
}

/// Truncates output to `MAX_OUTPUT_BYTES`, converting to lossy UTF-8.
fn truncate_output(bytes: &[u8]) -> String {
    if bytes.len() <= MAX_OUTPUT_BYTES {
        String::from_utf8_lossy(bytes).to_string()
    } else {
        let truncated = String::from_utf8_lossy(&bytes[..MAX_OUTPUT_BYTES]);
        format!("{truncated}\n... (output truncated at {MAX_OUTPUT_BYTES} bytes)")
    }
}

/// Maps file extensions to language config keys.
fn extension_to_language(ext: &str) -> Option<&'static str> {
    match ext {
        "py" => Some("python"),
        "rs" => Some("rust"),
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Some("javascript"),
        "go" => Some("go"),
        "java" => Some("java"),
        "rb" => Some("ruby"),
        "c" | "cpp" | "cc" | "cxx" | "h" | "hpp" => Some("c"),
        "php" => Some("php"),
        "cs" => Some("csharp"),
        "swift" => Some("swift"),
        "kt" | "kts" => Some("kotlin"),
        "zig" => Some("zig"),
        "ex" | "exs" => Some("elixir"),
        "hs" => Some("haskell"),
        "lua" => Some("lua"),
        "dart" => Some("dart"),
        "scala" | "sc" => Some("scala"),
        "r" | "R" => Some("r"),
        "jl" => Some("julia"),
        _ => None,
    }
}

/// Parses a timeout value from JSON, accepting both integer and string types.
fn parse_timeout(value: &serde_json::Value) -> Result<u64> {
    match value {
        serde_json::Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| anyhow!("timeout must be a positive integer")),
        serde_json::Value::String(s) => s
            .parse::<u64>()
            .map_err(|_| anyhow!("timeout must be a positive integer, got '{s}'")),
        _ => Err(anyhow!("timeout must be an integer or string")),
    }
}

impl LspBridgeHandler {
    /// Handles the `run` tool call.
    pub(super) fn handle_run(
        &self,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult> {
        let input: RunInput =
            serde_json::from_value(arguments.ok_or_else(|| anyhow!("Missing arguments"))?)
                .map_err(|e| anyhow!("Invalid arguments: {e}"))?;

        // Split command string if it contains spaces (e.g., "cargo test --lib")
        let (command, args) = if input.command.contains(' ') {
            let mut parts = input.command.split_whitespace();
            let cmd = parts.next().unwrap_or_default().to_string();
            let mut split_args: Vec<String> = parts.map(String::from).collect();
            split_args.extend(input.args);
            (cmd, split_args)
        } else {
            (input.command, input.args)
        };

        // Coerce timeout from string or integer
        let timeout_secs = input.timeout.as_ref().map(parse_timeout).transpose()?;

        debug!("run: {} {:?}", command, args);

        let run_tool = self
            .run_tool
            .as_ref()
            .ok_or_else(|| anyhow!("run tool is not configured"))?;

        // Validate and resolve cwd
        let cwd = if let Some(ref cwd_str) = input.cwd {
            let cwd_path = Self::resolve_path(cwd_str)?;
            let canonical = self
                .runtime
                .block_on(self.path_validator.read())
                .validate_read(&cwd_path)?;
            if !canonical.is_dir() {
                return Err(anyhow!("cwd is not a directory: {cwd_str}"));
            }
            Some(canonical)
        } else {
            None
        };

        let output = self.runtime.block_on(async {
            if let Some(secs) = input.sleep {
                if secs <= 0.0 || secs > 300.0 {
                    return Err(anyhow!("sleep must be between 0 and 300 seconds"));
                }
                tokio::time::sleep(std::time::Duration::from_secs_f64(secs)).await;
            }
            let manager = run_tool.read().await;
            manager.validate_command(&command, &args)?;
            manager
                .execute(
                    &command,
                    &args,
                    timeout_secs,
                    cwd.as_deref(),
                    input.stdin.as_deref(),
                )
                .await
        })?;

        // Handle output_file: write stdout to file if requested
        let stdout_text = if let Some(ref output_path) = input.output_file {
            let path = Self::resolve_path(output_path)?;
            let canonical = self
                .runtime
                .block_on(self.path_validator.read())
                .validate_write(&path)?;

            if let Some(parent) = canonical.parent() {
                self.runtime
                    .block_on(tokio::fs::create_dir_all(parent))
                    .map_err(|e| anyhow!("Failed to create parent directories: {e}"))?;
            }

            let byte_count = output.stdout.len();
            self.runtime
                .block_on(tokio::fs::write(&canonical, &output.stdout))
                .map_err(|e| anyhow!("Failed to write output file: {e}"))?;

            let rel_path = self.relative_display_path(&canonical);
            format!("Output written to {rel_path} ({byte_count} bytes)")
        } else {
            output.stdout.clone()
        };

        let mut result = String::new();

        if output.timed_out {
            let _ = writeln!(result, "TIMED OUT");
        }

        if let Some(code) = output.exit_code {
            let _ = writeln!(result, "Exit code: {code}");
        }

        if !stdout_text.is_empty() {
            let _ = writeln!(result, "\n{stdout_text}");
        }

        if !output.stderr.is_empty() {
            let _ = writeln!(result, "\nstderr:\n{}", output.stderr);
        }

        if result.is_empty() {
            result = "Command completed with no output.".to_string();
        }

        if output.timed_out || output.exit_code.is_some_and(|c| c != 0) {
            Ok(CallToolResult::error(result))
        } else {
            Ok(CallToolResult::text(result))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LanguageCommands, RunToolConfig};
    use std::fs;
    use tempfile::TempDir;

    fn make_config(allowed: &[&str], languages: &[(&str, &[&str])]) -> RunToolConfig {
        make_config_with_denied(allowed, &[], languages)
    }

    fn make_config_with_denied(
        allowed: &[&str],
        denied: &[&str],
        languages: &[(&str, &[&str])],
    ) -> RunToolConfig {
        let mut lang_map = HashMap::new();
        for (name, cmds) in languages {
            lang_map.insert(
                (*name).to_string(),
                LanguageCommands {
                    allowed: cmds.iter().map(|s| (*s).to_string()).collect(),
                },
            );
        }
        RunToolConfig {
            allowed: allowed.iter().map(|s| (*s).to_string()).collect(),
            denied: denied.iter().map(|s| (*s).to_string()).collect(),
            languages: lang_map,
        }
    }

    #[test]
    fn test_validate_base_allowed() -> Result<()> {
        let config = make_config(&["git", "make"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        manager.validate_command("git", &[])?;
        manager.validate_command("make", &[])?;
        assert!(manager.validate_command("rm", &[]).is_err());
        Ok(())
    }

    #[test]
    fn test_validate_denied_includes_allowlist() -> Result<()> {
        let config = make_config(&["git"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        let result = manager.validate_command("rm", &[]);
        assert!(result.is_err());
        let err = result
            .err()
            .ok_or_else(|| anyhow!("expected error"))?
            .to_string();
        assert!(err.contains("'rm'"), "Should mention denied command: {err}");
        assert!(err.contains("git"), "Should include allowlist: {err}");
        Ok(())
    }

    #[test]
    fn test_validate_unrestricted() -> Result<()> {
        let config = make_config(&["*"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        manager.validate_command("anything", &[])?;
        manager.validate_command("rm", &[])?;
        Ok(())
    }

    #[test]
    fn test_language_detection() -> Result<()> {
        let dir = TempDir::new()?;
        let root = dir.path().to_path_buf();

        // Create Python file
        fs::write(root.join("main.py"), "print('hello')")?;

        let config = make_config(&["git"], &[("python", &["python", "pytest"])]);
        let manager = RunToolManager::new(&config, &[root]);

        assert!(manager.detected_languages.contains("python"));
        manager.validate_command("python", &[])?;
        manager.validate_command("pytest", &[])?;
        Ok(())
    }

    #[test]
    fn test_language_not_detected_commands_denied() -> Result<()> {
        let dir = TempDir::new()?;
        let root = dir.path().to_path_buf();

        // No Python files — only Rust
        fs::write(root.join("main.rs"), "fn main() {}")?;

        let config = make_config(
            &["git"],
            &[("python", &["python", "pytest"]), ("rust", &["cargo"])],
        );
        let manager = RunToolManager::new(&config, &[root]);

        // Rust detected, Python not
        manager.validate_command("cargo", &[])?;
        assert!(manager.validate_command("python", &[]).is_err());
        Ok(())
    }

    #[test]
    fn test_describe_allowlist() -> Result<()> {
        let config = make_config(&["git", "make"], &[("python", &["python", "pytest"])]);
        let dir = TempDir::new()?;
        fs::write(dir.path().join("app.py"), "")?;
        let manager = RunToolManager::new(&config, &[dir.path().to_path_buf()]);

        let desc = manager.describe_allowlist();
        assert!(desc.contains("git"), "Should mention git: {desc}");
        assert!(desc.contains("make"), "Should mention make: {desc}");
        assert!(
            desc.contains("python (detected)"),
            "Should mention detected python: {desc}"
        );
        Ok(())
    }

    #[test]
    fn test_update_roots_changes_detection() -> Result<()> {
        let dir1 = TempDir::new()?;
        let dir2 = TempDir::new()?;

        // Only dir2 has Python files
        fs::write(dir2.path().join("app.py"), "")?;

        let config = make_config(&["git"], &[("python", &["python"])]);
        let mut manager = RunToolManager::new(&config, &[dir1.path().to_path_buf()]);

        assert!(!manager.detected_languages.contains("python"));

        // Update roots to include dir2
        let changed = manager.update_roots(&[dir1.path().to_path_buf(), dir2.path().to_path_buf()]);

        assert!(changed, "Should detect change");
        assert!(manager.detected_languages.contains("python"));
        Ok(())
    }

    #[tokio::test]
    async fn test_execute_echo() -> Result<()> {
        let config = make_config(&["echo"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        let output = manager
            .execute("echo", &["hello".to_string()], Some(5), None, None)
            .await?;

        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("hello"));
        assert!(!output.timed_out);
        Ok(())
    }

    #[tokio::test]
    async fn test_execute_timeout() -> Result<()> {
        let config = make_config(&["sleep"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        let output = manager
            .execute("sleep", &["10".to_string()], Some(1), None, None)
            .await?;

        assert!(output.timed_out);
        assert!(output.exit_code.is_none());
        Ok(())
    }

    #[test]
    fn test_parse_timeout_integer() -> Result<()> {
        let val = serde_json::json!(60);
        assert_eq!(parse_timeout(&val)?, 60);
        Ok(())
    }

    #[test]
    fn test_parse_timeout_string() -> Result<()> {
        let val = serde_json::json!("120");
        assert_eq!(parse_timeout(&val)?, 120);
        Ok(())
    }

    #[test]
    fn test_parse_timeout_invalid_string() {
        let val = serde_json::json!("abc");
        assert!(parse_timeout(&val).is_err());
    }

    #[test]
    fn test_parse_timeout_negative() {
        let val = serde_json::json!(-5);
        assert!(parse_timeout(&val).is_err());
    }

    #[tokio::test]
    async fn test_execute_with_cwd() -> Result<()> {
        let dir = TempDir::new()?;
        let config = make_config(&["pwd"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        let output = manager
            .execute("pwd", &[], Some(5), Some(dir.path()), None)
            .await?;

        assert_eq!(output.exit_code, Some(0));
        let canonical = dir.path().canonicalize()?;
        assert!(
            output.stdout.trim() == canonical.to_string_lossy(),
            "Expected cwd {:?}, got {:?}",
            canonical,
            output.stdout.trim()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_execute_with_stdin() -> Result<()> {
        let config = make_config(&["cat"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        let output = manager
            .execute("cat", &[], Some(5), None, Some("hello from stdin"))
            .await?;

        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.stdout, "hello from stdin");
        Ok(())
    }

    #[tokio::test]
    async fn test_run_with_sleep() -> Result<()> {
        let config = make_config(&["echo"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        let start = std::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_secs_f64(0.1)).await;
        let output = manager
            .execute("echo", &["after-sleep".to_string()], Some(5), None, None)
            .await?;
        let elapsed = start.elapsed();

        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("after-sleep"));
        assert!(
            elapsed >= std::time::Duration::from_millis(100),
            "Expected at least 100ms delay, got {elapsed:?}"
        );
        Ok(())
    }

    #[test]
    fn test_denied_subcommand_blocks() -> Result<()> {
        let config = make_config_with_denied(&["git"], &["git grep"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        let result = manager.validate_command("git", &["grep".into(), "pattern".into()]);
        assert!(result.is_err());
        let err = result
            .err()
            .ok_or_else(|| anyhow!("expected error"))?
            .to_string();
        assert!(
            err.contains("'git grep'"),
            "Should mention denied pair: {err}"
        );
        Ok(())
    }

    #[test]
    fn test_denied_does_not_affect_other_subcommands() -> Result<()> {
        let config = make_config_with_denied(&["git"], &["git grep"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        manager.validate_command("git", &["status".into()])?;
        Ok(())
    }

    #[test]
    fn test_denied_overrides_unrestricted() -> Result<()> {
        let config = make_config_with_denied(&["*"], &["git grep"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        assert!(
            manager.validate_command("git", &["grep".into()]).is_err(),
            "git grep should be denied even in unrestricted mode"
        );
        manager.validate_command("git", &["status".into()])?;
        manager.validate_command("rm", &[])?;
        Ok(())
    }

    #[test]
    fn test_denied_no_args_still_allowed() -> Result<()> {
        let config = make_config_with_denied(&["git"], &["git grep"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        manager.validate_command("git", &[])?;
        Ok(())
    }

    #[test]
    fn test_maybe_detect_language_new() {
        let config = make_config(&["git"], &[("rust", &["cargo"])]);
        let mut manager = RunToolManager::new(&config, &[]);

        assert!(!manager.detected_languages.contains("rust"));
        assert!(manager.maybe_detect_language(Path::new("src/main.rs")));
        assert!(manager.detected_languages.contains("rust"));
        // cargo should now be allowed
        assert!(manager.validate_command("cargo", &[]).is_ok());
    }

    #[test]
    fn test_maybe_detect_language_already_detected() -> Result<()> {
        let config = make_config(&["git"], &[("rust", &["cargo"])]);
        let dir = TempDir::new()?;
        let rs_file = dir.path().join("main.rs");
        fs::write(&rs_file, "fn main() {}")?;

        let mut manager = RunToolManager::new(&config, &[dir.path().to_path_buf()]);
        assert!(manager.detected_languages.contains("rust"));
        // Already detected — should return false
        assert!(!manager.maybe_detect_language(Path::new("src/lib.rs")));
        Ok(())
    }

    #[test]
    fn test_maybe_detect_language_unconfigured() {
        let config = make_config(&["git"], &[]);
        let mut manager = RunToolManager::new(&config, &[]);

        // Python not in language configs — should return false
        assert!(!manager.maybe_detect_language(Path::new("script.py")));
    }

    #[test]
    fn test_maybe_detect_language_no_extension() {
        let config = make_config(&["git"], &[("rust", &["cargo"])]);
        let mut manager = RunToolManager::new(&config, &[]);

        assert!(!manager.maybe_detect_language(Path::new("Makefile")));
    }
}

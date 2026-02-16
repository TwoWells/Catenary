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

        let mut manager = Self {
            base_allowed,
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

    /// Validates that a command is on the allowlist.
    ///
    /// # Errors
    ///
    /// Returns an error with the current allowlist if the command is denied.
    pub fn validate_command(&self, command: &str) -> Result<()> {
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
        if self.unrestricted {
            return "All commands are allowed.".to_string();
        }

        let mut parts = Vec::new();

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
            let manager = run_tool.read().await;
            manager.validate_command(&command)?;
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
            languages: lang_map,
        }
    }

    #[test]
    fn test_validate_base_allowed() {
        let config = make_config(&["git", "make"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        assert!(manager.validate_command("git").is_ok());
        assert!(manager.validate_command("make").is_ok());
        assert!(manager.validate_command("rm").is_err());
    }

    #[test]
    fn test_validate_denied_includes_allowlist() {
        let config = make_config(&["git"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        let err = manager.validate_command("rm").unwrap_err().to_string();
        assert!(err.contains("'rm'"), "Should mention denied command: {err}");
        assert!(err.contains("git"), "Should include allowlist: {err}");
    }

    #[test]
    fn test_validate_unrestricted() {
        let config = make_config(&["*"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        assert!(manager.validate_command("anything").is_ok());
        assert!(manager.validate_command("rm").is_ok());
    }

    #[test]
    fn test_language_detection() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();

        // Create Python file
        fs::write(root.join("main.py"), "print('hello')").unwrap();

        let config = make_config(&["git"], &[("python", &["python", "pytest"])]);
        let manager = RunToolManager::new(&config, &[root]);

        assert!(manager.detected_languages.contains("python"));
        assert!(manager.validate_command("python").is_ok());
        assert!(manager.validate_command("pytest").is_ok());
    }

    #[test]
    fn test_language_not_detected_commands_denied() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();

        // No Python files — only Rust
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        let config = make_config(
            &["git"],
            &[("python", &["python", "pytest"]), ("rust", &["cargo"])],
        );
        let manager = RunToolManager::new(&config, &[root]);

        // Rust detected, Python not
        assert!(manager.validate_command("cargo").is_ok());
        assert!(manager.validate_command("python").is_err());
    }

    #[test]
    fn test_describe_allowlist() {
        let config = make_config(&["git", "make"], &[("python", &["python", "pytest"])]);
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("app.py"), "").unwrap();
        let manager = RunToolManager::new(&config, &[dir.path().to_path_buf()]);

        let desc = manager.describe_allowlist();
        assert!(desc.contains("git"), "Should mention git: {desc}");
        assert!(desc.contains("make"), "Should mention make: {desc}");
        assert!(
            desc.contains("python (detected)"),
            "Should mention detected python: {desc}"
        );
    }

    #[test]
    fn test_update_roots_changes_detection() {
        let dir1 = TempDir::new().unwrap();
        let dir2 = TempDir::new().unwrap();

        // Only dir2 has Python files
        fs::write(dir2.path().join("app.py"), "").unwrap();

        let config = make_config(&["git"], &[("python", &["python"])]);
        let mut manager = RunToolManager::new(&config, &[dir1.path().to_path_buf()]);

        assert!(!manager.detected_languages.contains("python"));

        // Update roots to include dir2
        let changed = manager.update_roots(&[dir1.path().to_path_buf(), dir2.path().to_path_buf()]);

        assert!(changed, "Should detect change");
        assert!(manager.detected_languages.contains("python"));
    }

    #[tokio::test]
    async fn test_execute_echo() {
        let config = make_config(&["echo"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        let output = manager
            .execute("echo", &["hello".to_string()], Some(5), None, None)
            .await
            .unwrap();

        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("hello"));
        assert!(!output.timed_out);
    }

    #[tokio::test]
    async fn test_execute_timeout() {
        let config = make_config(&["sleep"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        let output = manager
            .execute("sleep", &["10".to_string()], Some(1), None, None)
            .await
            .unwrap();

        assert!(output.timed_out);
        assert!(output.exit_code.is_none());
    }

    #[test]
    fn test_parse_timeout_integer() {
        let val = serde_json::json!(60);
        assert_eq!(parse_timeout(&val).unwrap(), 60);
    }

    #[test]
    fn test_parse_timeout_string() {
        let val = serde_json::json!("120");
        assert_eq!(parse_timeout(&val).unwrap(), 120);
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
    async fn test_execute_with_cwd() {
        let dir = TempDir::new().unwrap();
        let config = make_config(&["pwd"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        let output = manager
            .execute("pwd", &[], Some(5), Some(dir.path()), None)
            .await
            .unwrap();

        assert_eq!(output.exit_code, Some(0));
        let canonical = dir.path().canonicalize().unwrap();
        assert!(
            output.stdout.trim() == canonical.to_string_lossy(),
            "Expected cwd {:?}, got {:?}",
            canonical,
            output.stdout.trim()
        );
    }

    #[tokio::test]
    async fn test_execute_with_stdin() {
        let config = make_config(&["cat"], &[]);
        let manager = RunToolManager::new(&config, &[]);

        let output = manager
            .execute("cat", &[], Some(5), None, Some("hello from stdin"))
            .await
            .unwrap();

        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.stdout, "hello from stdin");
    }
}

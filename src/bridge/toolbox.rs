// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Shared application container for tool servers and cross-tool infrastructure.
//!
//! `Toolbox` creates and owns all internal servers and shared dependencies.
//! Protocol boundaries (`LspBridgeHandler`, `HookServer`) hold `Arc<Toolbox>`
//! and access any dependency through it.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::RwLock;

use super::diagnostics_server::DiagnosticsServer;
use super::editing_manager::EditingManager;
use super::file_tools::GlobServer;
use super::filesystem_manager::FilesystemManager;
use super::grep_server::GrepServer;
use super::handler::expand_tilde;
use super::path_security::PathValidator;
use crate::config::Config;
use crate::config::SeverityConfig;
use crate::logging::LoggingServer;
use crate::logging::notification_queue::NotificationQueueSink;
use crate::lsp::LspClientManager;
use crate::lsp::glob::LspGlob;
use crate::ts::TsIndex;

/// A resolved glob pattern that handles tilde expansion and absolute paths.
///
/// For relative patterns (e.g. `src/**/*.rs`), matches against paths relative
/// to workspace roots. For absolute patterns (e.g. `~/other-project/*.rs`),
/// extracts the non-glob base directory as a search root and matches against
/// full paths.
pub struct ResolvedGlob {
    glob: LspGlob,
    match_full_path: bool,
    override_root: Option<PathBuf>,
}

impl ResolvedGlob {
    /// Resolves a glob pattern, expanding tilde and detecting absolute patterns.
    ///
    /// # Errors
    ///
    /// Returns an error if the pattern is not a valid glob.
    pub fn new(pattern: &str) -> Result<Self> {
        let expanded = expand_tilde(pattern);
        let glob = LspGlob::new(&expanded)?;

        if Path::new(&expanded).is_absolute() {
            let base = Self::base_dir(&expanded);
            Ok(Self {
                glob,
                match_full_path: true,
                override_root: Some(base),
            })
        } else {
            Ok(Self {
                glob,
                match_full_path: false,
                override_root: None,
            })
        }
    }

    /// Tests whether a file path matches this glob.
    ///
    /// For absolute patterns, matches against the full path.
    /// For relative patterns, strips the root prefix first.
    #[must_use]
    pub fn is_match(&self, path: &Path, root: &Path) -> bool {
        if self.match_full_path {
            self.glob.is_match(path)
        } else {
            let rel = path.strip_prefix(root).unwrap_or(path);
            self.glob.is_match(rel)
        }
    }

    /// Returns the override search root for absolute patterns.
    #[must_use]
    pub fn override_root(&self) -> Option<&Path> {
        self.override_root.as_deref()
    }

    /// Extracts the longest directory prefix without glob metacharacters.
    fn base_dir(pattern: &str) -> PathBuf {
        let mut base = PathBuf::new();
        for component in Path::new(pattern).components() {
            let s = component.as_os_str().to_string_lossy();
            if s.contains('*') || s.contains('?') || s.contains('[') || s.contains('{') {
                break;
            }
            base.push(component);
        }
        if base.as_os_str().is_empty() {
            PathBuf::from("/")
        } else {
            base
        }
    }
}

/// Shared application container for tool servers and cross-tool infrastructure.
///
/// Creates and owns all internal servers and shared dependencies.
/// [`super::handler::LspBridgeHandler`] holds an `Arc<Toolbox>` and handles
/// protocol boundary concerns (health checks, readiness, dispatch routing).
pub struct Toolbox {
    /// Grep tool server.
    pub grep: GrepServer,
    /// Glob tool server.
    pub glob: GlobServer,
    /// Diagnostics pipeline for `PostToolUse` hook requests.
    pub diagnostics: Arc<DiagnosticsServer>,
    /// In-memory editing state (`start_editing`/`done_editing` lifecycle).
    pub editing: EditingManager,
    /// LSP client manager (also owns document manager).
    pub(super) client_manager: Arc<LspClientManager>,
    /// File classification and root resolution.
    fs_manager: Arc<FilesystemManager>,
    /// Path validation for LSP-aware operations.
    path_validator: Arc<RwLock<PathValidator>>,
    /// Multi-sink tracing dispatcher.
    pub logging: LoggingServer,
    /// Notification queue for draining into `systemMessage`.
    pub notifications: Arc<NotificationQueueSink>,
    /// Tree-sitter symbol index (shared with grep).
    pub ts_index: Option<Arc<std::sync::Mutex<TsIndex>>>,
    /// Catenary instance ID (unique per process invocation).
    pub instance_id: Arc<str>,
    /// Tokio runtime handle for blocking dispatch.
    pub runtime: Handle,
}

impl Toolbox {
    /// Creates a new `Toolbox`, constructing all internal dependencies.
    ///
    /// Constructs the logging sinks and activates the `LoggingServer`,
    /// draining any bootstrap-buffered events. After this call, all
    /// `tracing` events flow through the logging pipeline.
    #[must_use]
    pub fn new(
        config: Config,
        roots: Vec<PathBuf>,
        logging: LoggingServer,
        conn: Arc<std::sync::Mutex<rusqlite::Connection>>,
        instance_id: Arc<str>,
        runtime: Handle,
    ) -> Self {
        // Construct logging sinks.
        let threshold = config
            .notifications
            .as_ref()
            .map_or_else(SeverityConfig::default, |n| n.threshold)
            .into();
        let notifications = NotificationQueueSink::new(threshold);
        let protocol_db =
            crate::logging::protocol_db::ProtocolDbSink::new(conn.clone(), instance_id.clone());
        let trace_db = crate::logging::trace_db::TraceDbSink::new(conn, instance_id.clone());

        // Activate — drains bootstrap buffer, enables direct dispatch.
        logging.activate(vec![notifications.clone(), protocol_db, trace_db]);

        let classification = super::filesystem_manager::ClassificationTables::from_config(&config);
        let fs_manager = Arc::new(FilesystemManager::with_classification(classification));
        fs_manager.set_roots(roots.clone());
        fs_manager.seed();

        // Build tree-sitter index (in-memory, no database dependency).
        let ts_index = TsIndex::build(&roots)
            .map(|idx| Arc::new(std::sync::Mutex::new(idx)))
            .map_err(|e| tracing::info!("tree-sitter index unavailable: {e}"))
            .ok();

        let grep_budget = config
            .tools
            .as_ref()
            .map_or(4000, |t| t.grep.budget as usize);
        let glob_budget = config
            .tools
            .as_ref()
            .map_or(2000, |t| t.glob.budget as usize);

        let path_validator = Arc::new(RwLock::new(PathValidator::new(roots)));
        let client_manager = Arc::new(LspClientManager::new(
            config,
            logging.clone(),
            fs_manager.clone(),
        ));
        let diagnostics = Arc::new(DiagnosticsServer::new(
            client_manager.clone(),
            path_validator.clone(),
        ));

        let grep = GrepServer {
            client_manager: client_manager.clone(),
            fs_manager: fs_manager.clone(),
            ts_index: ts_index.clone(),
            budget: grep_budget,
        };
        let glob = GlobServer {
            client_manager: client_manager.clone(),
            fs_manager: fs_manager.clone(),
            budget: glob_budget,
        };
        Self {
            grep,
            glob,
            diagnostics,
            editing: EditingManager::new(),
            client_manager,
            fs_manager,
            path_validator,
            logging,
            notifications,
            ts_index,
            instance_id,
            runtime,
        }
    }

    /// Returns `true` if the path is within any known workspace root.
    ///
    /// Simple prefix check against known roots — no canonicalization or
    /// symlink resolution. Used for hook scope gating where approximate
    /// checking is sufficient.
    #[must_use]
    pub fn is_within_roots(&self, path: &Path) -> bool {
        self.fs_manager.resolve_root(path).is_some()
    }

    /// Diffs the filesystem and notifies servers with matching file watcher
    /// registrations. Delegates to [`LspClientManager::notify_file_changes`].
    pub async fn notify_file_changes(&self) {
        self.client_manager.notify_file_changes().await;
    }

    /// Spawns LSP servers for languages detected in the workspace.
    pub async fn spawn_all(&self) {
        self.client_manager.spawn_all().await;
    }

    /// Synchronizes workspace roots with a new set.
    ///
    /// Updates path validation, notifies LSP servers of folder changes,
    /// and spawns servers for any newly detected languages.
    ///
    /// # Errors
    ///
    /// Returns an error if root synchronization fails.
    pub async fn sync_roots(&self, roots: Vec<PathBuf>) -> Result<()> {
        // sync_roots updates FilesystemManager roots first (before any
        // async work), then reacts to the diff.
        self.client_manager.sync_roots(roots.clone()).await?;
        self.path_validator.write().await.update_roots(roots);

        // Fire-and-forget: spawn_all is pre-warming, not a gate.
        // Tool calls that need a server will trigger spawning on demand.
        let cm = self.client_manager.clone();
        tokio::spawn(async move { cm.spawn_all().await });
        Ok(())
    }

    /// Shuts down all active LSP servers gracefully.
    pub async fn shutdown(&self) {
        self.client_manager.shutdown_all().await;
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;
    use std::path::Path;

    // ── expand_tilde ──────────────────────────────────────────────

    #[test]
    fn expand_tilde_home_prefix() {
        let home = std::env::var("HOME").expect("HOME must be set");
        assert_eq!(expand_tilde("~/foo/bar"), format!("{home}/foo/bar"));
    }

    #[test]
    fn expand_tilde_bare() {
        let home = std::env::var("HOME").expect("HOME must be set");
        assert_eq!(expand_tilde("~"), home);
    }

    #[test]
    fn expand_tilde_no_op_for_absolute() {
        assert_eq!(expand_tilde("/usr/bin"), "/usr/bin");
    }

    #[test]
    fn expand_tilde_no_op_for_relative() {
        assert_eq!(expand_tilde("src/main.rs"), "src/main.rs");
    }

    #[test]
    fn expand_tilde_no_op_for_mid_tilde() {
        assert_eq!(expand_tilde("foo/~/bar"), "foo/~/bar");
    }

    // ── ResolvedGlob::base_dir ────────────────────────────────────

    #[test]
    fn base_dir_strips_at_star() {
        let base = ResolvedGlob::base_dir("/home/user/projects/*");
        assert_eq!(base, Path::new("/home/user/projects"));
    }

    #[test]
    fn base_dir_strips_at_double_star() {
        let base = ResolvedGlob::base_dir("/home/user/**/*.rs");
        assert_eq!(base, Path::new("/home/user"));
    }

    #[test]
    fn base_dir_strips_at_question_mark() {
        let base = ResolvedGlob::base_dir("/tmp/foo?/bar");
        assert_eq!(base, Path::new("/tmp"));
    }

    #[test]
    fn base_dir_strips_at_bracket() {
        let base = ResolvedGlob::base_dir("/tmp/[abc]/bar");
        assert_eq!(base, Path::new("/tmp"));
    }

    #[test]
    fn base_dir_no_metachar_returns_full_path() {
        let base = ResolvedGlob::base_dir("/home/user/projects/src");
        assert_eq!(base, Path::new("/home/user/projects/src"));
    }

    #[test]
    fn base_dir_only_metachar_returns_root() {
        let base = ResolvedGlob::base_dir("*");
        assert_eq!(base, Path::new("/"));
    }

    // ── ResolvedGlob::new ─────────────────────────────────────────

    #[test]
    fn resolved_glob_relative_pattern() {
        let rg = ResolvedGlob::new("src/**/*.rs").expect("valid glob");
        assert!(rg.override_root().is_none());
        assert!(!rg.match_full_path);
    }

    #[test]
    fn resolved_glob_absolute_pattern() {
        let rg = ResolvedGlob::new("/tmp/project/*.rs").expect("valid glob");
        assert_eq!(rg.override_root(), Some(Path::new("/tmp/project")));
        assert!(rg.match_full_path);
    }

    #[test]
    fn resolved_glob_tilde_becomes_absolute() {
        let rg = ResolvedGlob::new("~/projects/*.rs").expect("valid glob");
        assert!(rg.override_root().is_some());
        assert!(rg.match_full_path);
    }

    #[test]
    fn resolved_glob_invalid_pattern() {
        assert!(ResolvedGlob::new("[invalid").is_err());
    }

    // ── ResolvedGlob::is_match ────────────────────────────────────

    #[test]
    fn is_match_relative_strips_root() {
        let rg = ResolvedGlob::new("src/**/*.rs").expect("valid glob");
        let root = Path::new("/workspace");

        assert!(rg.is_match(Path::new("/workspace/src/lib.rs"), root));
        assert!(rg.is_match(Path::new("/workspace/src/deep/mod.rs"), root));
        assert!(!rg.is_match(Path::new("/workspace/tests/foo.rs"), root));
    }

    #[test]
    fn is_match_relative_star_no_cross_directory() {
        let rg = ResolvedGlob::new("src/*.rs").expect("valid glob");
        let root = Path::new("/workspace");

        assert!(rg.is_match(Path::new("/workspace/src/lib.rs"), root));
        assert!(!rg.is_match(Path::new("/workspace/src/deep/mod.rs"), root));
    }

    #[test]
    fn is_match_absolute_uses_full_path() {
        let rg = ResolvedGlob::new("/tmp/project/*.rs").expect("valid glob");
        let root = Path::new("/tmp/project");

        assert!(rg.is_match(Path::new("/tmp/project/main.rs"), root));
        // `*` does not cross directory boundaries (shell-like)
        assert!(!rg.is_match(Path::new("/tmp/project/sub/lib.rs"), root));
        assert!(!rg.is_match(Path::new("/other/main.rs"), root));
    }

    #[test]
    fn is_match_absolute_double_star() {
        let rg = ResolvedGlob::new("/tmp/project/**/*.rs").expect("valid glob");
        let root = Path::new("/tmp/project");

        assert!(rg.is_match(Path::new("/tmp/project/main.rs"), root));
        assert!(rg.is_match(Path::new("/tmp/project/sub/lib.rs"), root));
        assert!(!rg.is_match(Path::new("/other/main.rs"), root));
    }

    #[test]
    fn is_match_relative_wrong_root_still_tries() {
        let rg = ResolvedGlob::new("*.txt").expect("valid glob");
        // When strip_prefix fails, falls back to matching the full path.
        // A bare filename matches *.txt.
        assert!(rg.is_match(Path::new("notes.txt"), Path::new("/nonexistent")));
    }
}

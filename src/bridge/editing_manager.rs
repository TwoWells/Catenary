// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! In-memory editing state manager.
//!
//! Tracks which agents are in editing mode and accumulates modified file
//! paths during editing sessions. State is per-session lifetime — no
//! database persistence needed since the session owns the [`super::toolbox::Toolbox`]
//! which owns the `EditingManager`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Result, anyhow};

/// In-memory editing state manager.
///
/// Owns editing state for a single Catenary session. Both
/// [`super::hook_router::HookRouter`] (which has the real `agent_id` from
/// the host CLI) and [`super::handler::McpRouter`] (which produces the
/// tool result) access this through [`super::toolbox::Toolbox`].
pub struct EditingManager {
    /// Active editing sessions: `agent_id` → accumulated file paths.
    state: Mutex<HashMap<String, Vec<PathBuf>>>,
}

impl Default for EditingManager {
    fn default() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
        }
    }
}

impl EditingManager {
    /// Creates a new `EditingManager` with no active editing sessions.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enters editing mode for an agent.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent is already in editing mode.
    pub fn start_editing(&self, agent_id: &str) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.contains_key(agent_id) {
            return Err(anyhow!("agent is already in editing mode"));
        }
        state.insert(agent_id.to_string(), Vec::new());
        drop(state);
        Ok(())
    }

    /// Returns `true` if the agent is currently in editing mode.
    #[must_use]
    pub fn is_editing(&self, agent_id: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(agent_id)
    }

    /// Accumulates a modified file path for an agent in editing mode.
    ///
    /// Idempotent — duplicate paths are not added.
    pub fn add_file(&self, agent_id: &str, path: PathBuf) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(files) = state.get_mut(agent_id)
            && !files.contains(&path)
        {
            files.push(path);
        }
    }

    /// Returns and clears accumulated file paths for an agent.
    pub fn drain_files(&self, agent_id: &str) -> Vec<PathBuf> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .get_mut(agent_id)
            .map(std::mem::take)
            .unwrap_or_default()
    }

    /// Exits editing mode for an agent, removing the entry entirely.
    pub fn done_editing(&self, agent_id: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.remove(agent_id);
    }

    /// Drains accumulated file paths from all agents and clears all
    /// editing state. Returns the combined file list.
    ///
    /// Used by the MCP `done_editing` tool, which does not carry an
    /// `agent_id` and cannot rely on [`active_agent`] to find the
    /// correct key.
    pub fn drain_all_and_clear(&self) -> Vec<PathBuf> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let files: Vec<PathBuf> = state.values_mut().flat_map(std::mem::take).collect();
        state.clear();
        files
    }

    /// Clears all editing state. Returns the number of entries removed.
    ///
    /// Used by `SessionStart` cleanup to clear stale state when the
    /// agent's context is reset.
    pub fn clear_all(&self) -> usize {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = state.len();
        state.clear();
        count
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn start_editing_enters_mode() {
        let em = EditingManager::new();
        em.start_editing("agent-a").expect("should succeed");
        assert!(em.is_editing("agent-a"));
    }

    #[test]
    fn start_editing_already_editing_errors() {
        let em = EditingManager::new();
        em.start_editing("agent-a").expect("first call");
        let err = em
            .start_editing("agent-a")
            .expect_err("should error on duplicate");
        assert!(
            err.to_string().contains("already in editing mode"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn is_editing_false_when_not_started() {
        let em = EditingManager::new();
        assert!(!em.is_editing("agent-a"));
    }

    #[test]
    fn add_file_accumulates() {
        let em = EditingManager::new();
        em.start_editing("").expect("start");
        em.add_file("", PathBuf::from("/src/main.rs"));
        em.add_file("", PathBuf::from("/src/lib.rs"));
        let files = em.drain_files("");
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn add_file_deduplicates() {
        let em = EditingManager::new();
        em.start_editing("").expect("start");
        em.add_file("", PathBuf::from("/src/main.rs"));
        em.add_file("", PathBuf::from("/src/main.rs"));
        let files = em.drain_files("");
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn add_file_ignored_when_not_editing() {
        let em = EditingManager::new();
        em.add_file("ghost", PathBuf::from("/src/main.rs"));
        assert!(em.drain_files("ghost").is_empty());
    }

    #[test]
    fn drain_files_clears() {
        let em = EditingManager::new();
        em.start_editing("").expect("start");
        em.add_file("", PathBuf::from("/src/main.rs"));
        let first = em.drain_files("");
        assert_eq!(first.len(), 1);
        let second = em.drain_files("");
        assert!(second.is_empty());
    }

    #[test]
    fn done_editing_removes_entry() {
        let em = EditingManager::new();
        em.start_editing("agent-a").expect("start");
        em.done_editing("agent-a");
        assert!(!em.is_editing("agent-a"));
        // Can re-enter after done
        em.start_editing("agent-a").expect("re-enter");
        assert!(em.is_editing("agent-a"));
    }

    #[test]
    fn drain_all_and_clear_collects_and_clears() {
        let em = EditingManager::new();
        em.start_editing("agent-a").expect("start");
        em.add_file("agent-a", PathBuf::from("/src/main.rs"));
        em.add_file("agent-a", PathBuf::from("/src/lib.rs"));

        let files = em.drain_all_and_clear();
        assert_eq!(files.len(), 2);
        assert!(!em.is_editing("agent-a"));

        // Empty when nothing is editing
        let files = em.drain_all_and_clear();
        assert!(files.is_empty());
    }

    #[test]
    fn clear_all_empties_state() {
        let em = EditingManager::new();
        em.start_editing("agent-a").expect("start a");
        em.start_editing("agent-b").expect("start b");
        em.add_file("agent-a", PathBuf::from("/src/main.rs"));
        let count = em.clear_all();
        assert_eq!(count, 2);
        assert!(!em.is_editing("agent-a"));
        assert!(!em.is_editing("agent-b"));
    }
}

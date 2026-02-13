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

//! Server state and progress tracking types.

use lsp_types::{NumberOrString, ProgressParams, ProgressParamsValue, WorkDoneProgress};
use serde::Serialize;
use std::collections::HashMap;
use std::time::Instant;

/// Token type for progress tracking (string or number).
pub type ProgressToken = NumberOrString;

/// State of an active progress operation.
#[derive(Debug, Clone)]
pub struct ProgressState {
    /// The title of the progress operation.
    pub title: String,
    /// The optional progress message.
    pub message: Option<String>,
    /// The optional progress percentage (0-100).
    pub percentage: Option<u32>,
    /// When the operation started.
    pub started: Instant,
}

/// Overall server readiness state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerState {
    /// Server just spawned, may be initializing.
    Initializing,
    /// Server actively indexing/processing.
    Indexing,
    /// Server ready to handle requests.
    Ready,
    /// Server connection lost.
    Dead,
}

impl ServerState {
    /// Create from atomic u8 value.
    #[must_use]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Initializing,
            1 => Self::Indexing,
            2 => Self::Ready,
            _ => Self::Dead,
        }
    }

    /// Convert to atomic u8 value.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Initializing => 0,
            Self::Indexing => 1,
            Self::Ready => 2,
            Self::Dead => 3,
        }
    }
}

/// Detailed status for a single LSP server.
#[derive(Debug, Clone, Serialize)]
pub struct ServerStatus {
    /// The language ID this server handles.
    pub language: String,
    /// Current server readiness state.
    pub state: ServerState,
    /// Active progress title, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_title: Option<String>,
    /// Active progress message, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_message: Option<String>,
    /// Active progress percentage, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_percentage: Option<u32>,
    /// Seconds since spawn.
    pub uptime_secs: u64,
}

/// Manages progress state for a single LSP client.
#[derive(Debug, Default)]
pub struct ProgressTracker {
    active_progress: HashMap<ProgressToken, ProgressState>,
}

impl ProgressTracker {
    /// Creates a new `ProgressTracker`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Update state from a progress notification.
    pub fn update(&mut self, params: &ProgressParams) {
        match &params.value {
            ProgressParamsValue::WorkDone(progress) => match progress {
                WorkDoneProgress::Begin(begin) => {
                    self.active_progress.insert(
                        params.token.clone(),
                        ProgressState {
                            title: begin.title.clone(),
                            message: begin.message.clone(),
                            percentage: begin.percentage,
                            started: Instant::now(),
                        },
                    );
                }
                WorkDoneProgress::Report(report) => {
                    if let Some(state) = self.active_progress.get_mut(&params.token) {
                        if report.message.is_some() {
                            state.message.clone_from(&report.message);
                        }
                        if report.percentage.is_some() {
                            state.percentage = report.percentage;
                        }
                    }
                }
                WorkDoneProgress::End(_) => {
                    self.active_progress.remove(&params.token);
                }
            },
        }
    }

    /// Returns true if server is busy with any progress operations.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        !self.active_progress.is_empty()
    }

    /// Returns the most significant active progress (longest running or lowest percentage).
    #[must_use]
    pub fn primary_progress(&self) -> Option<&ProgressState> {
        self.active_progress
            .values()
            .min_by_key(|p| p.percentage.unwrap_or(0))
    }

    /// Clear all progress (e.g., on reconnect).
    pub fn clear(&mut self) {
        self.active_progress.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, Result};

    fn make_progress_params(token: &str, progress: WorkDoneProgress) -> ProgressParams {
        ProgressParams {
            token: NumberOrString::String(token.to_string()),
            value: ProgressParamsValue::WorkDone(progress),
        }
    }

    #[test]
    fn test_progress_begin_end() -> Result<()> {
        let mut tracker = ProgressTracker::new();
        assert!(!tracker.is_busy());

        // Begin progress
        let begin = make_progress_params(
            "indexing",
            WorkDoneProgress::Begin(lsp_types::WorkDoneProgressBegin {
                title: "Indexing".to_string(),
                cancellable: None,
                message: Some("src/main.rs".to_string()),
                percentage: Some(0),
            }),
        );
        tracker.update(&begin);

        assert!(tracker.is_busy());
        let primary = tracker.primary_progress().context("missing progress")?;
        assert_eq!(primary.title, "Indexing");
        assert_eq!(primary.message, Some("src/main.rs".to_string()));
        assert_eq!(primary.percentage, Some(0));

        // End progress
        let end = make_progress_params(
            "indexing",
            WorkDoneProgress::End(lsp_types::WorkDoneProgressEnd { message: None }),
        );
        tracker.update(&end);

        assert!(!tracker.is_busy());
        Ok(())
    }

    #[test]
    fn test_progress_report() -> Result<()> {
        let mut tracker = ProgressTracker::new();

        // Begin
        let begin = make_progress_params(
            "indexing",
            WorkDoneProgress::Begin(lsp_types::WorkDoneProgressBegin {
                title: "Indexing".to_string(),
                cancellable: None,
                message: None,
                percentage: Some(0),
            }),
        );
        tracker.update(&begin);

        // Report progress
        let report = make_progress_params(
            "indexing",
            WorkDoneProgress::Report(lsp_types::WorkDoneProgressReport {
                cancellable: None,
                message: Some("50% done".to_string()),
                percentage: Some(50),
            }),
        );
        tracker.update(&report);

        let primary = tracker.primary_progress().context("missing progress")?;
        assert_eq!(primary.percentage, Some(50));
        assert_eq!(primary.message, Some("50% done".to_string()));
        Ok(())
    }

    #[test]
    fn test_multiple_progress_tokens() -> Result<()> {
        let mut tracker = ProgressTracker::new();

        // Begin two progress operations
        let begin1 = make_progress_params(
            "indexing",
            WorkDoneProgress::Begin(lsp_types::WorkDoneProgressBegin {
                title: "Indexing".to_string(),
                cancellable: None,
                message: None,
                percentage: Some(50),
            }),
        );
        let begin2 = make_progress_params(
            "analyzing",
            WorkDoneProgress::Begin(lsp_types::WorkDoneProgressBegin {
                title: "Analyzing".to_string(),
                cancellable: None,
                message: None,
                percentage: Some(10),
            }),
        );
        tracker.update(&begin1);
        tracker.update(&begin2);

        assert!(tracker.is_busy());

        // Primary should be the one with lower percentage
        let primary = tracker.primary_progress().context("missing progress")?;
        assert_eq!(primary.title, "Analyzing");
        assert_eq!(primary.percentage, Some(10));

        // End one
        let end1 = make_progress_params(
            "indexing",
            WorkDoneProgress::End(lsp_types::WorkDoneProgressEnd { message: None }),
        );
        tracker.update(&end1);

        assert!(tracker.is_busy());
        let primary = tracker.primary_progress().context("missing progress")?;
        assert_eq!(primary.title, "Analyzing");
        Ok(())
    }

    #[test]
    fn test_server_state_conversion() {
        assert_eq!(ServerState::from_u8(0), ServerState::Initializing);
        assert_eq!(ServerState::from_u8(1), ServerState::Indexing);
        assert_eq!(ServerState::from_u8(2), ServerState::Ready);
        assert_eq!(ServerState::from_u8(3), ServerState::Dead);
        assert_eq!(ServerState::from_u8(99), ServerState::Dead);

        assert_eq!(ServerState::Initializing.as_u8(), 0);
        assert_eq!(ServerState::Indexing.as_u8(), 1);
        assert_eq!(ServerState::Ready.as_u8(), 2);
        assert_eq!(ServerState::Dead.as_u8(), 3);
    }
}

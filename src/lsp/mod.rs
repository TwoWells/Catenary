// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

/// Low-level LSP client for communicating with a server process.
pub mod client;
/// High-level manager for lazy-spawning and caching LSP clients.
pub mod manager;
/// LSP message protocol definitions.
pub mod protocol;
/// Server state and progress tracking.
pub mod state;

pub(crate) use client::DIAGNOSTICS_TIMEOUT;
pub use client::LspClient;
pub use manager::{ClientManager, detect_workspace_languages};
pub use state::{ProgressTracker, ServerState, ServerStatus};

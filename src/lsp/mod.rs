// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

/// Low-level LSP client for communicating with a server process.
pub mod client;
/// Diagnostics strategy selection and activity monitoring.
pub(crate) mod diagnostics;
/// High-level manager for lazy-spawning and caching LSP clients.
pub mod manager;
/// LSP message protocol definitions.
pub mod protocol;
/// Server state and progress tracking.
pub mod state;

pub(crate) use client::{DIAGNOSTICS_TIMEOUT, DiagnosticsWaitResult};
pub use client::{LspClient, WARMUP_PERIOD};
pub use manager::{ClientManager, detect_workspace_languages};
pub use state::{ProgressTracker, ServerState, ServerStatus};

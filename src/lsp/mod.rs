// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

/// Low-level LSP client for communicating with a server process.
pub mod client;
/// Transport layer: process lifecycle, reader loop, request/response correlation.
pub(crate) mod connection;
/// Diagnostics strategy selection and activity monitoring.
pub(crate) mod diagnostics;
#[allow(dead_code, reason = "Wired into production code in phase 0d")]
/// Extractor functions for LSP response and notification fields.
pub(crate) mod extract;
/// Shared server state and notification dispatch.
pub(crate) mod inbox;
/// High-level manager for lazy-spawning and caching LSP clients.
pub mod manager;
#[allow(dead_code, reason = "Wired into production code in phase 0d")]
/// Builder functions for LSP request and notification parameters.
pub(crate) mod params;
/// LSP message protocol definitions.
pub mod protocol;
/// Server state and progress tracking.
pub mod state;
#[allow(dead_code, reason = "Wired into production code in phase 0d")]
/// Small local types for LSP concepts.
pub(crate) mod types;
/// Unified wait infrastructure for load-aware failure detection.
pub(crate) mod wait;

pub use client::DiagnosticsWaitResult;
pub use client::{LspClient, WARMUP_PERIOD};
pub use manager::{ClientManager, detect_workspace_languages};
pub use state::{ProgressTracker, ServerState, ServerStatus};

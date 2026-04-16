// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

/// Low-level LSP client for communicating with a server process.
pub mod client;
/// Transport layer: process lifecycle, reader loop, request/response correlation.
pub(crate) mod connection;
/// Extractor functions for LSP response and notification fields.
pub(crate) mod extract;
/// LSP file watcher glob patterns and change types.
pub mod glob;
/// Standalone pure functions for LSP document identity.
pub mod lang;
/// High-level manager for lazy-spawning and caching LSP clients.
pub mod manager;
/// Builder functions for LSP request and notification parameters.
pub(crate) mod params;
/// LSP message protocol definitions.
pub mod protocol;
/// Server profile: init-time capabilities and runtime observations.
pub(crate) mod server;
/// Idle detection and profiling: polls process tree for quiet detection.
pub mod settle;
/// Server state and progress tracking.
pub mod state;
/// Small local types for LSP concepts.
pub(crate) mod types;

pub use client::LspClient;
pub use manager::LspClientManager;
pub use server::LspServer;
pub use state::{ProgressTracker, ServerLifecycle, ServerStatus};

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
pub use manager::ClientManager;
pub use state::{ProgressTracker, ServerState, ServerStatus};

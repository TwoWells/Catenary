// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Interactive TUI for browsing sessions and tailing events.
//!
//! Two-pane layout with focus tracking:
//! - **Top pane**: scrollable session list with active/dead indicators and
//!   language servers.
//! - **Bottom pane**: live, colored event tail for the selected session
//!   with a scrollbar.
//!
//! All colors use the terminal's ANSI palette so the TUI inherits whatever
//! theme the user has configured.

pub mod app;
pub mod data;
pub mod filter;
pub mod grid;
pub mod layout;
pub mod mouse;
pub mod panel;
pub mod scrollbar;
pub mod selection;
pub mod theme;
pub mod tree;

pub use app::App;
pub use data::{DataSource, MockDataSource};

use anyhow::Result;

use crate::config::IconConfig;

/// Run the interactive TUI.
///
/// # Errors
///
/// Returns an error if terminal setup fails or session data cannot be read.
pub fn run(_icon_config: IconConfig) -> Result<()> {
    // Minimal stub — full implementation in ticket 11.
    Ok(())
}

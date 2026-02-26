// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Application state for the TUI.
//!
//! This module contains the core [`App`] struct and the [`FocusedPane`]
//! enum. Later tickets will add input modes, layout state, and more fields.

use super::data::DataSource;
use super::theme::{IconSet, Theme};

/// Which region has keyboard focus.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FocusedPane {
    /// The sessions list pane.
    Sessions,
    /// The events detail pane.
    Events,
}

/// Application state driving the TUI.
pub struct App {
    /// Semantic color theme.
    pub theme: Theme,
    /// Resolved icon theme.
    pub icons: IconSet,
    /// Which pane currently has focus.
    pub focus: FocusedPane,
    /// Data source for session and event data.
    pub data: Box<dyn DataSource>,
    /// Whether the user wants to quit.
    pub quit: bool,
}

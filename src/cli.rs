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

//! CLI utilities for terminal output formatting and colors.

use crossterm::tty::IsTty;
use std::io::stdout;

/// Configuration for color output
#[derive(Debug, Clone)]
pub struct ColorConfig {
    pub enabled: bool,
}

impl ColorConfig {
    /// Create a new ColorConfig, auto-detecting TTY unless nocolor is true
    pub fn new(nocolor: bool) -> Self {
        Self {
            enabled: !nocolor && stdout().is_tty(),
        }
    }

    /// ANSI escape code for green (incoming/request)
    pub fn green(&self, s: &str) -> String {
        if self.enabled {
            format!("\x1b[32m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    }

    /// ANSI escape code for blue (outgoing/response)
    pub fn blue(&self, s: &str) -> String {
        if self.enabled {
            format!("\x1b[34m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    }

    /// ANSI escape code for red (errors)
    pub fn red(&self, s: &str) -> String {
        if self.enabled {
            format!("\x1b[31m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    }

    /// ANSI escape code for cyan (language names)
    pub fn cyan(&self, s: &str) -> String {
        if self.enabled {
            format!("\x1b[36m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    }

    /// ANSI escape code for dim text
    pub fn dim(&self, s: &str) -> String {
        if self.enabled {
            format!("\x1b[2m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    }
}

/// Get the terminal width, defaulting to 80 if unable to detect
pub fn terminal_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
}

/// Truncate a string to max_len characters, adding "..." if truncated
pub fn truncate(s: &str, max_len: usize) -> String {
    if max_len <= 3 {
        return ".".repeat(max_len.min(3));
    }
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len - 3])
    }
}

/// Column width configuration for the list command
#[derive(Debug)]
pub struct ColumnWidths {
    pub row_num: usize,
    pub id: usize,
    pub pid: usize,
    pub workspace: usize,
    pub client: usize,
    pub languages: usize,
    pub started: usize,
}

impl ColumnWidths {
    /// Calculate column widths based on terminal width
    /// Columns: # | ID | PID | WORKSPACE | CLIENT | LANGUAGES | STARTED
    pub fn calculate(term_width: usize) -> Self {
        // Fixed minimum widths
        let row_num = 3; // "#"
        let pid = 8; // "PID"
        let started = 12; // "STARTED"

        // Calculate flexible widths
        // Reserve space for separators (6 spaces between columns)
        let fixed_space = row_num + pid + started + 6;
        let flexible_space = term_width.saturating_sub(fixed_space);

        // Distribute flexible space: ID(12), WORKSPACE(flex), CLIENT(20), LANGUAGES(15)
        let min_id = 12;
        let min_client = 15;
        let min_languages = 10;
        let min_workspace = 20;

        let total_min_flex = min_id + min_client + min_languages + min_workspace;

        if flexible_space <= total_min_flex {
            // Use minimum widths
            Self {
                row_num,
                id: min_id,
                pid,
                workspace: min_workspace,
                client: min_client,
                languages: min_languages,
                started,
            }
        } else {
            // Distribute extra space primarily to workspace
            let extra = flexible_space - total_min_flex;
            Self {
                row_num,
                id: min_id,
                pid,
                workspace: min_workspace + extra,
                client: min_client,
                languages: min_languages,
                started,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("test", 4), "test");
    }

    #[test]
    fn test_truncate_long_string() {
        assert_eq!(truncate("hello world", 8), "hello...");
        assert_eq!(truncate("abcdefghij", 7), "abcd...");
    }

    #[test]
    fn test_truncate_edge_cases() {
        assert_eq!(truncate("hello", 3), "...");
        assert_eq!(truncate("hello", 2), "..");
        assert_eq!(truncate("hello", 1), ".");
        assert_eq!(truncate("hello", 0), "");
    }

    #[test]
    fn test_color_config_disabled() {
        let config = ColorConfig::new(true);
        assert!(!config.enabled);
        assert_eq!(config.green("test"), "test");
        assert_eq!(config.blue("test"), "test");
        assert_eq!(config.red("test"), "test");
        assert_eq!(config.cyan("test"), "test");
    }

    #[test]
    fn test_calculate_column_widths() {
        let widths = ColumnWidths::calculate(120);
        assert_eq!(widths.row_num, 3);
        assert_eq!(widths.pid, 8);
        assert_eq!(widths.started, 12);
        // Flexible columns should have reasonable widths
        assert!(widths.id >= 12);
        assert!(widths.workspace >= 20);
        assert!(widths.client >= 15);
        assert!(widths.languages >= 10);
    }

    #[test]
    fn test_calculate_column_widths_shrinks() {
        let widths = ColumnWidths::calculate(60);
        // Should use minimum widths for narrow terminals
        assert_eq!(widths.id, 12);
        assert_eq!(widths.workspace, 20);
        assert_eq!(widths.client, 15);
        assert_eq!(widths.languages, 10);
    }
}

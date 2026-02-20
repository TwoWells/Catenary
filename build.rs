// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Build script that embeds git version info into the binary.
//!
//! Runs `git describe --tags --always --dirty` to produce version strings like:
//! - `1.3.6` (on a tagged commit)
//! - `1.3.6-3-gabc1234` (3 commits past a tag)
//! - `1.3.6-3-gabc1234-dirty` (uncommitted changes)
//!
//! Falls back to `CARGO_PKG_VERSION` if git is unavailable.

use std::process::Command;

fn main() {
    // Rebuild when the git HEAD changes (new commit, checkout, etc.)
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");

    // Rebuild when hook definitions change (embedded via include_str!)
    println!("cargo:rerun-if-changed=plugins/catenary/hooks/hooks.json");
    println!("cargo:rerun-if-changed=hooks/hooks.json");

    let version = git_describe().unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=CATENARY_VERSION={version}");
}

fn git_describe() -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let desc = String::from_utf8(output.stdout).ok()?;
    let desc = desc.trim();

    if desc.is_empty() {
        return None;
    }

    // Strip leading 'v' from tags like v1.3.6
    Some(desc.strip_prefix('v').unwrap_or(desc).to_string())
}

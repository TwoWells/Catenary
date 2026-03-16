// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

fn main() {
    let x: i32 = "oops";
    drop(x);
}

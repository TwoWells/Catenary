// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Safe FFI wrapper for loading tree-sitter grammars from shared libraries.
//!
//! Tree-sitter grammars compile to shared libraries (`.so`/`.dylib`/`.dll`) that
//! export a C function returning a `Language` pointer. This crate wraps the unsafe
//! FFI needed to load those libraries and extract the language, exposing a safe API
//! to the main crate (which forbids unsafe code).
//!
//! The loaded library is intentionally leaked — grammar shared libraries live for
//! the process lifetime and are never unloaded.

use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::ffi::TSLanguage;

/// Load a tree-sitter grammar from a compiled shared library.
///
/// Loads the shared library at `lib_path` and looks up the symbol named
/// `symbol_name` (e.g., `"tree_sitter_rust"`). The symbol must be an
/// `extern "C"` function returning a `*const ()` that can be converted
/// to a [`tree_sitter::Language`].
///
/// The loaded library is intentionally leaked — it must remain mapped for
/// the returned `Language` to remain valid. Grammars are loaded once at
/// session start and live for the process lifetime.
///
/// # Errors
///
/// Returns an error if the shared library cannot be loaded or the symbol
/// is not found.
pub fn load_grammar(lib_path: &Path, symbol_name: &str) -> Result<tree_sitter::Language> {
    // Safety: `libloading::Library::new` loads a shared library from disk.
    // The library must be a valid shared object. We trust the path because
    // it comes from the grammar registry (installed via `catenary install`).
    let lib = unsafe { libloading::Library::new(lib_path) }
        .with_context(|| format!("failed to load grammar library: {}", lib_path.display()))?;

    // Safety: We look up a symbol that should be an `extern "C" fn() -> *const TSLanguage`
    // following the tree-sitter grammar convention. The symbol name comes from
    // the grammar registry.
    let func: libloading::Symbol<'_, unsafe extern "C" fn() -> *const TSLanguage> = unsafe {
        lib.get(symbol_name.as_bytes())
    }
    .with_context(|| format!("symbol '{symbol_name}' not found in {}", lib_path.display()))?;

    // Safety: Calling the grammar's constructor function. All tree-sitter
    // grammars follow this convention — the function returns a pointer to
    // a static `TSLanguage` struct.
    let raw_ptr = unsafe { func() };

    // Safety: `from_raw` constructs a `Language` from the raw pointer returned
    // by the grammar's constructor. The pointer is valid for the lifetime of
    // the loaded library, which we leak below.
    let language = unsafe { tree_sitter::Language::from_raw(raw_ptr) };

    // Leak the library so the grammar code stays mapped. The Language holds
    // a pointer into the library's memory — dropping the library would
    // invalidate it.
    std::mem::forget(lib);

    Ok(language)
}

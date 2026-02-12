# Catenary Agent Context

This file serves as the single point of truth for AI agents (Claude, Gemini, etc.) working on the Catenary project.

## Project Grounding
- **Project Goal:** High-performance multiplexing bridge between MCP and LSP.
- **Config:** `@./Cargo.toml`
- **Dependency Policy:** `@./deny.toml`
- **Documentation:** `docs/src/`

## Coding Standards
- **Edition:** Rust 2024.
- **Safety:** `unsafe` code is strictly forbidden (`forbid(unsafe_code)`).
- **Error Handling:** Use `anyhow` for application logic and `thiserror` for library errors.
- **Strict Denials:** Do NOT use `unwrap()`, `expect()`, `panic!()`, `todo!()`, `unimplemented!()`, `dbg!()`, `println!()`, or `eprintln!()`. Use proper error handling and the `tracing` crate for logging.
- **Imports:** No wildcard imports (`use crate::*`).
- **Formatting:** Code must be formatted with `rustfmt`.
- **Linting:** Must pass `cargo clippy` with `pedantic`, `nursery`, and `cargo` groups enabled.

## Quality Standards
- **License Compliance:** All new dependencies MUST have permissive licenses (MIT, Apache-2.0, etc.) as specified in `@./deny.toml`. Catenary is dual-licensed GPL-3.0 and Commercial.
- **Documentation:** All public APIs must have documentation comments.
- **Testing:**
  - All new features must include tests.
  - Integration tests in `tests/` often require real LSP servers (e.g., `rust-analyzer`).

## Development Commands
- **Build:** `cargo build`
- **Test:** `cargo test`
- **Lint:** `cargo clippy`
- **Format:** `cargo fmt`

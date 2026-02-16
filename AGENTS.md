# Catenary Agent Context

This file serves as the single point of truth for AI agents (Claude, Gemini, etc.) working on the Catenary project.

## Project Grounding
- **Project Goal:** High-performance multiplexing bridge between MCP and LSP.
- **Repository:** `MarkWells-Dev/Catenary` on GitHub.
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

## Release Workflow
Versioning and releases are managed via the `Makefile`.
- **Patch Release:** `make release-patch` (e.g., 0.1.0 -> 0.1.1)
- **Minor Release:** `make release-minor` (e.g., 0.1.0 -> 0.2.0)
- **Major Release:** `make release-major` (e.g., 0.1.0 -> 1.0.0)
- **Custom Version:** `make release V=x.y.z`

These commands automatically:
1. Verify the working tree is clean and on `main`.
2. Bump versions in `Cargo.toml` and `.claude-plugin/marketplace.json`.
3. Run all tests and linting checks.
4. Commit the changes and create a git tag.

To complete the release, push the changes and tags:
`git push && git push --tags`

### Pre-release checklist
Before running `make release-*`:
1. Run `cargo update` to ensure `Cargo.lock` is fresh. The release
   commit's pre-commit hook runs `cargo-lock-check --locked`, which
   fails if any dependency has a newer compatible version available.
2. Ensure `git push` has been run so local `main` matches `origin/main`.

If checks or the commit fail, the Makefile automatically rolls back
the version bump — it is safe to re-run `make release-*` after fixing
the issue.

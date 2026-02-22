# Catenary Agent Context

This file serves as the single point of truth for AI agents (Claude, Gemini, etc.) working on the Catenary project.

## Project Grounding
- **Project Goal:** High-performance multiplexing bridge between MCP and LSP.
- **Repository:** `MarkWells-Dev/Catenary` on GitHub.
- **Config:** `@./Cargo.toml`
- **Dependency Policy:** `@./deny.toml`
- **Documentation:** `docs/src/`

## How Catenary Works

Catenary is an MCP server that gives AI agents LSP-powered code intelligence. It
multiplexes one or more LSP servers (e.g., rust-analyzer, typescript-language-server)
behind a single MCP interface, providing hover, go-to-definition, diagnostics,
find-references, rename, and search without shell-based text scanning.

### Core concepts

- **Session:** A running Catenary instance. Each session has a unique ID (opaque
  string), a PID, and one or more workspace roots. Sessions are discoverable via
  `catenary list` and monitorable via `catenary monitor <id>`. See `src/session.rs`.
- **MCP tools:** The tools exposed to agents (search, hover, definition,
  diagnostics, etc.) are defined in the MCP server. Each tool delegates to one or
  more LSP servers under the hood.
- **Hooks:** Catenary integrates with host CLIs (Claude Code, Gemini CLI) via
  hooks that fire before/after tool use. Hook definitions live in
  `plugins/catenary/hooks/hooks.json` (Claude Code) and `hooks/hooks.json`
  (Gemini CLI). See `docs/src/plugin-architecture.md` for the full hook contract.
- **File locking and diagnostics:** The `catenary acquire` / `catenary release`
  commands (`src/lock.rs` for locking, `src/notify.rs` for diagnostics) manage
  the full pre/post-tool lifecycle. Locks are advisory, filesystem-based, and
  keyed by absolute file path. Ownership is tracked by an `owner` string built
  from `session_id` (+ `agent_id` if present) from the hook JSON.
  - `catenary acquire` (PreToolUse on Edit/Write/Read): blocks until the lock is
    available. Also runs stale-read detection — compares the file's current mtime
    against the last tracked value for this owner, and warns if they differ.
  - `catenary release` (PostToolUse on Edit/Write/Read): runs diagnostics notify,
    records the file's mtime (track-read), then releases the lock with a grace
    period (default 30s) so the same owner can re-acquire without contention
    during diagnostics→fix cycles. On failure (`--grace 0`, no `--format`), just
    releases the lock immediately.
- **Root sync:** `catenary sync-roots` (PreToolUse, Claude Code only) scans the
  transcript for `/add-dir` workspace additions and forwards them to the session.

### Architecture references

- `docs/src/plugin-architecture.md` — plugin layout, hook contracts, version management.
- `src/lock.rs` — file locking and read tracking implementation.
- `src/session.rs` — session lifecycle and event broadcasting.
- `docs/src/` — full documentation source.

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
- **Check (full):** `make check` — format, lint, deny, and test in one pass.
- **Test (all):** `make test`
- **Test (filtered):** `make test T=<filter>` — run only tests matching the filter (e.g., `make test T=json_diagnostics`).
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
2. Run `cargo update` to ensure `Cargo.lock` is fresh.
3. Bump versions in `Cargo.toml` and `.claude-plugin/marketplace.json`.
4. Run all tests and linting checks.
5. Commit the changes and create a git tag.

To complete the release, push the changes and tags:
`git push && git push --tags`

### Pre-release checklist
Before running `make release-*`:
1. Ensure `git push` has been run so local `main` matches `origin/main`.

If checks or the commit fail, the Makefile automatically rolls back
the version bump — it is safe to re-run `make release-*` after fixing
the issue.

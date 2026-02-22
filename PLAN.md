# Plan: Catenary Monitor TUI & Log Management

This plan outlines the transition of Catenary's observability tools to an
interactive Terminal User Interface (TUI) and improved, persistent log
management.

## Objectives

- **User-First CLI**: `catenary` launches the TUI when invoked interactively;
  serves MCP when stdin and stdout are both non-TTY pipes (TTY detection).
- **No extra subcommand**: Users never need to type `catenary serve`. TTY
  detection handles dispatch automatically. No `--serve` flag.
- **7-Day Persistence**: Retain session logs for 7 days by default to aid
  post-mortem debugging.
- **Unified Interface**: Combine session listing and event monitoring into a
  single TUI.
- **Configurable Retention**: `log_retention_days` in `catenary.toml`.

## Decision Log

| Decision | Choice | Rationale |
|---|---|---|
| Retention default | 7 days | Reasonable post-mortem window |
| Persistence special values | `0` = none, `-1` = forever | |
| MCP dispatch | TTY detection on stdin + stdout | Robust; no flag needed |
| `--serve` flag | None | TTY detection is sufficient; a flag encourages workarounds |
| stderr in TTY check | Excluded | MCP clients don't control stderr; including it causes false positives |
| mockls in release builds | Excluded via `required-features` | Test-only binary; never needed in production |

## Progress

### Housekeeping
- [x] Gate `mockls` behind `required-features = ["mockls"]` so it is excluded
      from release builds
- [x] Update CI (`ci.yml`) and `Makefile` to pass `--features mockls` for
      build, clippy, and test steps
- [x] Add `--no-fail-fast` to nextest in `make check`

### Phase 1 — Config & Session Persistence
- [x] Add `log_retention_days: i64` to `Config` (default: 7; env:
      `CATENARY_LOG_RETENTION_DAYS`)
- [x] Stop deleting session directories in `Session::Drop`
- [x] Write a `dead` marker file on drop so observers can detect clean shutdown
      without relying solely on PID liveness (which is vulnerable to PID reuse)
- [x] Add `session_is_alive(dir, pid)`: checks dead marker first, falls back to
      PID check for crash recovery
- [x] TTY detection in `main.rs`: dispatch to MCP server when
      `!stdin.is_terminal() && !stdout.is_terminal()`, otherwise launch TUI
      (interim: `run_dashboard` calls `run_list` until Phase 2)
- [ ] Add `prune_sessions(retention_days: i64)` to remove directories older
      than the configured threshold

### Phase 2 — TUI Scaffolding
- [ ] Add `ratatui` dependency (crossterm already present)
- [ ] Create `src/tui.rs` with two-pane layout skeleton
- [ ] Verify `crossterm` version is compatible with chosen `ratatui` version

### Phase 3 — Session Browser (Top Pane)
- [ ] Scrollable session list with Active/Dead status indicators
- [ ] `j`/`k` navigation

### Phase 4 — Event Tailer (Bottom Pane)
- [ ] Live tail of selected session's `events.jsonl`
- [ ] Works for both active and dead sessions
- [ ] `f` to focus/filter by agent or lock-owner

### Phase 5 — Pruning
- [ ] Call `prune_sessions` on TUI startup
- [ ] Respect `log_retention_days` from config
- [ ] `x` keybind in TUI to manually delete a dead session log

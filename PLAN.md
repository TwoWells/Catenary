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

### constrained-bash hook (`scripts/constrained_bash.py`)
- [x] Move script into repo (`scripts/constrained_bash.py`); symlink replaces
      `~/.claude/hooks/constrained_bash.py`
- [x] Add `--format=claude|gemini` flag for dual-host support
- [x] Pipeline-safe allowlist: `grep`/`head`/`tail`/`wc`/`jq` allowed
      mid-pipeline (reading stdin), denied at pipeline start (reading files)
- [x] Recursive `$()` and backtick checking so subshell commands are validated
- [x] Heredoc exception: `cat`/`head`/`tail` with `<<` allowed (reading stdin)
- [x] Add `jq` to `PIPELINE_SAFE`; `sleep` to `ALLOWED`
- [x] Unit tests (`scripts/test_constrained_bash.py`) including adversarial
      cases; `make test-scripts` target
- [x] Update `docs/src/cli-integration.md` with symlink install instructions
      and `$HOME`-based paths (tilde not reliably expanded in hook commands)
- [x] `catenary doctor` constrained-bash check — implementation notes:
      - Embed the canonical script at compile time:
        `const CONSTRAINED_BASH_EXPECTED: &str = include_str!("../scripts/constrained_bash.py");`
      - Add `--diff` flag to the `Doctor` subcommand in the CLI struct
      - Add `check_constrained_bash_claude(colors, show_diff)` and
        `check_constrained_bash_gemini(colors, show_diff)` — mirrors the
        existing `check_claude_hooks` / `check_gemini_hooks` pattern in
        `src/main.rs`
      - Detection: parse `~/.claude/settings.json` and `~/.gemini/settings.json`
        as JSON; walk all hook command strings looking for any that contain
        `"constrained_bash.py"`; extract the script path (first whitespace-
        delimited token of the command value); expand `$HOME`
      - If not found in either file: print dimmed `- not configured` and return
        (opt-in feature — absence is not an error)
      - If found: read the file at the resolved path, compare to
        `CONSTRAINED_BASH_EXPECTED`. Match → `✓ up to date`. Mismatch →
        `✗ out of date (run catenary doctor --diff to see changes)`
      - `--diff`: when set, show a unified diff using the `similar` crate
        (MIT licensed — permitted by `deny.toml`). Apply to all stale checks
        (hooks.json files and constrained_bash.py) so one flag shows everything.
      - Wire both checks into `run_doctor` after the existing hooks section,
        under a `Scripts:` header to mirror the `Hooks:` header
- [x] Debug hook not blocking: root cause was missing `chmod +x` on the repo
      script — Claude Code receives a permission error, treats it as a
      non-blocking hook failure, and lets the command through
- [x] Fix deny response: remove `systemMessage` (caused "hook error" display
      conflating with the block); add `suppressOutput: true`; add back
      `systemMessage` with the blocked command for visibility
- [x] Add `TestDenyResponse` unit tests covering response shape (no stale
      `systemMessage` format, `suppressOutput` present, correct gemini format)
- [x] Add `cargo build 2>&1` to `TestDenied.test_cargo` — regression for
      stderr-redirect not masking the blocked command
- [x] Document troubleshooting in `docs/src/cli-integration.md`: `chmod +x`
      requirement, shell profile stdout contamination fix
      (`[[ -o interactive ]] || return`), and Bash→Read UI masquerade behaviour

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
- [x] Add `prune_sessions(retention_days: i64)` to remove directories older
      than the configured threshold
- [x] Fix session ID collision: replace tid_hash with atomic counter so
      multiple sessions created in the same millisecond get unique IDs

### Phase 2 — TUI Scaffolding
- [x] Add `ratatui 0.30` dependency (`crossterm_0_28` feature; 0.28/0.29
      failed cargo-deny due to unmaintained `paste` advisory RUSTSEC-2024-0436)
- [x] Create `src/tui.rs` with two-pane layout skeleton
- [x] Verify `crossterm` version is compatible with chosen `ratatui` version
- [x] Add `deny` target to Makefile for isolated license/advisory checks

### Phase 3 — Session Browser (Top Pane)
- [x] Scrollable session list with Active/Dead status indicators
- [x] `j`/`k` navigation
- [x] `r` to refresh session list

### Phase 4 — Event Tailer (Bottom Pane)
- [x] Live tail of selected session's `events.jsonl`
- [x] Works for both active and dead sessions
- [x] `f` to focus/filter events (case-insensitive substring match on
      formatted event line; `F` clears; Enter applies; Esc cancels input)

### Phase 5 — Pruning
- [x] Call `prune_sessions` on TUI startup (in `run_dashboard`)
- [x] Respect `log_retention_days` from config
- [x] `x` keybind in TUI to manually delete a dead session log
      (refuses active sessions; shows status message on success/failure)

### Phase 6 — TUI Polish
- [x] Show active language servers as a dim second line under each session row
- [x] Separate keybinding hints into a footer bar (out of panel titles)
- [x] Footer says `x delete log` to clarify it only removes log data
- [x] Only load last N events on session switch (avoids replay flash)
- [x] Colored events using ratatui `Span` + `Style` (matches
      `print_event_annotated` color scheme: dim timestamps, cyan languages,
      green/blue tool arrows, red errors, yellow warnings)
- [x] Nerd Font icons for events:
      -  / / / diagnostics (error/warn/info/ok)
      -  lock /  unlock
      -  search /  map /  hover /  goto /  refs /  diagnostics
      - `◆` server state / `⟳` progress / `◇` MCP / `●`/`○` start/stop

# Catenary Internal

Private planning and tracking for the Catenary project.

This is a separate git repository nested inside the main Catenary repo.
It is excluded via `.gitignore` and has its own remote and commit history.

## Workstreams

| # | Workstream | Status | Tracker |
|---|-----------|--------|---------|
| 1 | [TUI rewrite](#1-tui-rewrite) | **Core complete** | `tickets/tui/README.md` |
| 2 | [MCP tool collapse](#2-mcp-tool-collapse) | **Complete** | `tickets/mcp/README.md` |
| 3 | [Lock removal + SQLite](#3-lock-removal--sqlite) | **Phase 2 complete** | `tickets/sql/README.md` |
| 4 | [SEARCHv2](#4-searchv2) | **In progress** | `tickets/searchv2/README.md` |
| 5 | [Misc](#5-misc) | **Open** | `tickets/misc/README.md` |
| 6 | [Wait model redesign](#6-wait-model-redesign) | **Reverted → v2 design** | `tickets/wait/DESIGN.md` |
| 7 | [Wait model v2](#7-wait-model-v2) | **1b blocked on acquire v1** | `tickets/waitv2/README.md` |
| 8 | [Monitoring](#8-monitoring) | **Complete** | `tickets/monitoring/README.md` |
| 9 | [Filtering](#9-filtering) | **Design complete** | `tickets/filtering/DESIGN.md` |
| 10 | [Collapse](#10-collapse) | **Complete** | `tickets/collapse/README.md` |
| 11 | [Replace](#11-replace) | **Superseded by 13** | `tickets/replace/README.md` |
| 12 | [Summarize](#12-summarize) | **Complete** | `tickets/summarize/README.md` |
| 13 | [Diagnostic batching](#13-diagnostic-batching) | **Ticket 00 done, 01 next** | `tickets/acquire/DESIGN.md` |
| 14 | [Recommend](#14-recommend) | **Design complete** | `tickets/recommend/DESIGN.md` |

## Current priority

**Workstream 13 (Diagnostic batching) v1 in progress.** Ticket 00
(schema + editing state operations) done. Ticket 01 (MCP tools:
`start_editing`/`done_editing`) next. v1 scope: editing state table,
MCP tools (no hold semantics, no mutual exclusion), PreToolUse deny,
PostToolUse suppression, Stop/AfterAgent enforcement. Cold release —
no LSP changes. Constrained bash rewrite decoupled to workstream 14
(Recommend). Replace removal is a cleanup ticket after v1 ships.

**Workstream 7 (Wait model v2) Phase 1b blocked on v1.**
1b agents benefit from diagnostic suppression during heavy refactors.
1b design change: `DocumentManager` retained as ref-counted document
lifecycle owner (not stripped to utilities). Multiple agents may have
overlapping files open via v2 warm state — `DocumentManager` tracks
ref counts per URI, sends `didOpen` on first open and `didClose` on
last close. Also: make `DiagnosticsServer` pipeline composable, add
`TextDocumentSyncKind` to `ServerProfile`.
Phase 0 (structural refactoring, 8 tickets) complete. Phase 1a
(protocol compliance, 8 tickets) complete — all profiling done
(findings 23-25), settle signal validated. 1a-04 deferred to 1b.
Phase 1b pipeline design finalized: `tickets/waitv2/design/pipeline_1b.md`.
Settle design superseded by pipeline_1b.md.
Phase 1b has 10 tickets (1b-00 through 1b-08, including 02a/02b split).
Critical path: 01 → 02a → 02b → 03 → {06, 07} → 08.
Independent: 00 (capability gates), 04 (OnceLock), 05 (dm utils).

**Workstream 4 (SEARCHv2) is in progress.** Ticket 00 complete. Next
eligible: 01, 02a, 03, 04, 05 (parallel). Blocked on capacity.

**Workstream 10 (Collapse) is complete.** All phases (0-5) done.

**Workstream 11 (Replace) is complete.** All 6 tickets (00a-00c,
01-03) landed. Superseded by acquire — replace tool, snapshots, and
restore CLI to be removed once acquire v1 ships.

---

## 1. TUI rewrite

**Status: Core complete, polish open.** Core rewrite (12 tickets) complete.
2 of 4 polish tickets done. Open: 13 (sessions scrollbar), 14 (snake
spinner). Done: 15 (workspace tree grouping), 16 (auto-close dead panels).

Rebuilt `catenary monitor` TUI from scratch: BSP layout, sessions
tree, events panels, expansion, grid, scrollbar, selection, filters,
mouse, responsive degradation, hints, yank support.

Source: `src/tui/`. Tracker: `tickets/tui/README.md`.

---

## 2. MCP tool collapse

**Status: Complete.** 15 tools collapsed to 2 (grep, glob) + hooks.
Shakedown signed off. Tool surface is stable.

19 tickets completed (00–14, 20–22). Ticket 10 (shakedown) produced
8 follow-ups; the critical ones (14, 20, 21, 22) landed. TUI polish
follow-ups (15–18) moved to workstream 1. Ticket 19 consolidated
into misc/03.

Tracker: `tickets/mcp/README.md`.

---

## 3. Lock removal + SQLite

**Status: Phase 2 complete.**

Two phases: remove filesystem advisory locks (phase 1), then replace
file-based session/event storage with SQLite (phase 2). Target
release: 2.0.0.

### Phase 1 — Lock removal (tickets 00-02) — Complete

- [x] 00 — Remove lock module
- [x] 01 — Remove lock event kinds
- [x] 02 — Update hooks and docs

### Phase 2 — SQLite migration (tickets 03-10) — Complete

- [x] 03 — Add rusqlite, create `src/db.rs`
- [x] 04 — Migrate session.rs
- [x] 05 — SQLite data source for TUI
- [x] 06 — Migrate main.rs commands
- [x] 07 — `catenary query` command
- [x] 08 — `catenary gc` command
- [x] 09 — Legacy data migration
- [x] 10 — SQLite documentation

### Phase 3 — Release (blocked by workstream 4)

- [ ] 11 — Version bump to 2.0.0

Tracker: `tickets/sql/README.md`.

---

## 4. SEARCHv2

**Status: In progress.** Tickets cut across 6 phases (00–10).
Ticket 00 (dependencies and SQLite schema) complete.

Replaces the current grep/glob implementation with tree-sitter as
the symbol source, navigation edges (calls, impls, supertypes,
subtypes), tiered output degradation with character budgets, and
tab-indented output. Adds glob structural navigation (`into`) and
defensive maps. Sed (Phase 4) pulled out to workstream 11 (Replace).

Design: `designs/SEARCHv2.md`, `designs/GLOBv2.md`.
Issues: `designs/ISSUES.md` (all resolved and applied).
Decisions: `decisions/001-006`.
Tracker: `tickets/searchv2/README.md`.

---

## 5. Misc

Cross-cutting decisions and items.

- [x] 01 — `Stuck` server state and recovery model
- [x] 02 — Thread `&Connection` for test DB isolation
- [x] 03 — Diagnostic noise filter trait
- [x] 04 — Session ID mismatch: Catenary vs Claude Code
- [x] 05 — Investigate: notify hook output for all cases
- [x] 06 — Filter rust-analyzer lint attribution diagnostics
- [x] 07 — Capture `client_session_id` at session start via `SessionStart` hook
- [x] 08 — Notify hook picks dead sessions + `generate_id` timestamp always zero
- [ ] 09 — Spawn missing LSP servers mid-session when new file types appear
- [ ] 10 — Yank strips percentage/count detail from `Progress` events
- [ ] 11 — Handle MCP `notifications/cancelled`: abort in-flight LSP requests
- [ ] 12 — Diagnostics budget: cap or summarize when 300+ diagnostics flood context (mitigated by workstream 13 acquire/release)
- [ ] 13 — Support `scopeUri` in `workspace/configuration` responses
- [ ] 18 — Split `panel.rs` into `pipeline.rs` + `panel.rs` + `flat.rs`
- [x] 14 — Kill child LSP processes on session exit
- [x] 17 — TUI keybinding overhaul: Space toggle + horizontal scroll

Tracker: `tickets/misc/README.md`.

---

## 6. Wait model redesign

**Status: Phase 2 complete.** Phases 0–2 done. Phase 3 (macOS/Windows
tree walking validation) blocked on Linux validation gate.

Replaced the two-strategy diagnostics wait system (`Version` /
`TokenMonitor`) and `load_aware_grace` with `pulse_monitor`: a single
hybrid wait loop using version match as the gate, process tree walking
and progress tokens as activity signals, and probe forks for
self-calibrating beat intervals. All wait sites (`wait_ready`, request
timeouts, diagnostics) now use `pulse_monitor`. `ServerState::Stuck`,
`try_idle_recover`, and `load_aware_grace` removed. Hook subcommands
unified under `catenary hook` with PreToolUse didOpen baseline and
per-file pending counters for parallel tool debounce.

Design: `tickets/wait/DESIGN.md`.

---

## 7. Wait model v2

**Status: Phase 1b blocked on acquire v1.** All 8 Phase 0 tickets
landed (0a, 0b, 0c, 0d1-0d4, 0e). Three-layer architecture
(Connection, ServerInbox, Client) in place, `lsp_types` removed, JSON
builders/extractors throughout. Phase 1a complete.

Phase 1b pipeline design finalized. 10 tickets (1b-00 through 1b-08,
including 02a/02b split). Blocked on acquire v1 — 1b agents need
diagnostic suppression during heavy refactors. 1b tickets need
updating to accommodate acquire v2:
- `DocumentManager` removal: strip down to `didChange` utilities
  instead of deleting entirely.
- `DiagnosticsServer`: make pipeline composable so release can enter
  at `didSave` (skipping `didOpen`).
- `ServerProfile`: add `TextDocumentSyncKind` during capability
  extraction.
- Per-interaction model: document three variants (diagnostics, tool
  request, acquire).

Design: `tickets/waitv2/design/OUTLINE.md`.
Pipeline design: `tickets/waitv2/design/pipeline_1b.md`.
Rejected design: `archive/designs/WAITV2_DESIGN_REJECTED.md`.
Tracker: `tickets/waitv2/README.md`.
LSP spec reference: `architecture/lsp/spec-reference.md`.

---

## 8. Monitoring

**Status: Complete.** All 4 tickets (01-04) landed.

Full LSP and MCP protocol visibility through the TUI. Logger trait
decouples capture from storage. Unified `ProtocolMessage` event type
replaces `McpMessage`. TUI switches to WAL-based change notification.
Session state derived from event stream.

Design: `tickets/monitoring/DESIGN.md`. Tracker: `tickets/monitoring/README.md`.

---

## 9. Filtering

**Status: Design complete.** Monitoring (workstream 8) is now complete,
so the `ProtocolMessage` dependency is satisfied. Ready for ticketing.

Modal filter panel system for the TUI with three scope levels (global,
workspace, session), inheritance between scopes, text search with recents,
and tree-structured expand/collapse for server and tool categories.

Design: `tickets/filtering/DESIGN.md`.

---

## 10. Collapse

**Status: Complete.** All phases (0-5) done. Rearchitects the event system around Catenary's identity as a
JSON bridge between three protocols (MCP, LSP, Hooks). Four architectural
layers, two cleanly separated concerns:

**Message layer (ground truth).** Replaces the `EventKind` enum with a
message envelope: `type` (mcp/lsp/hook), `method`, `server`, `client`,
`request_id`, `parent_id`, and raw `payload`. Three message types — one
per protocol boundary, nothing invented. Three boundary components own
logging: `McpServer`, `LspClient` (collapses `Connection` + `ServerInbox`
+ `LspClient`), `HookServer` (renamed `NotifyServer`). `ToolServer`
trait (replaces `LspBridgeHandler`) is the transformation layer — a
black box that does not log. `MessageLog` (renamed `EventBroadcaster`)
replaces the `Logger` trait and all implementations. Cross-message
correlation via two integer foreign keys: `request_id` (pair merge) and
`parent_id` (scope/causation).

**Display layer (derived view).** TUI reads messages from the database
and surfaces timing relationships visually. Two-pass pipeline: pair merge
(join on `request_id`), then run collapse (consecutive same-category
messages → single line). Category grouping driven by `lsp_category()` /
`mcp_category()` / `hook_category()`.

Design: `tickets/collapse/DESIGN.md`.
Issues: `tickets/collapse/ISSUES.md`.
Tracker: `tickets/collapse/README.md`.

---

## 11. Replace

**Status: Superseded by workstream 13 (Acquire).** All 6 tickets
(00a-00c, 01-03) landed and shipped. MCP tool to be removed in
workstream 13.

Batch edit MCP tool. Accepted a glob pattern and a list of
`{old, new, flags}` edit operations. Applied all edits in one tool
call, created a snapshot before modification, returned consolidated
LSP diagnostics after all edits landed. `catenary restore` CLI for
point-in-time file recovery.

The tool solved the diagnostic firehose problem but required
voluntary adoption — agents never used it because they prefer the
host's Edit tool (trained behavior). Acquire/release (workstream 13)
solves the same problem by working with the agent's training:
the agent uses Edit, the hook system manages diagnostic timing.

The `ReplaceServer`, MCP tool registration, snapshot infrastructure
(`snapshots` table, `catenary restore`, sidecar logic), and all
related code are removed in workstream 13.

Design: `designs/REPLACE.md` (superseded by `designs/ACQUIRE.md`).
Tracker: `tickets/replace/README.md`.

---

## 12. Summarize

**Status: Complete.** All 11 tickets across 5 phases (00a–08) done.

Inverts the TUI display hierarchy. Summary lines surface the innermost
useful content (error messages, result counts, diagnostic severity) instead
of protocol scaffolding (direction arrows, method names, JSON-RPC structure).
Icons replace direction arrows — an icon carries at-a-glance semantic status
(success, protocol error, cancellation, progress) while direction is implied
by protocol role. Scope collapse via `parent_id` groups hundreds of LSP
messages from a single tool call behind one summary line.

Builds on collapse (workstream 10) infrastructure. Adds a new pipeline
pass (`scope_collapse`) and rewrites all formatters. 11 tickets across
5 phases: foundation (theme.rs split + icon config), structural prerequisites
(`parent_id` propagation), scope collapse (basic + segmented), formatter
rewrite (singles/collapsed + pairs), expansion model (frontmatter), summary
metrics, run collapse at depth, and dead code removal.

Design: `tickets/summarize/DESIGN.md`. Tracker: `tickets/summarize/README.md`.

---

## 13. Diagnostic batching

**Status: Ticket 00 done, 01 next.**

Per-file diagnostic suppression via `start_editing`/`done_editing`
MCP tools. The agent signals intent to edit a file; diagnostics are
deferred until the agent signals completion. No mutual exclusion —
multiple agents can edit the same file simultaneously. Courtesy
messages inform other agents when a file's diagnostics are deferred.
Forced adoption via PreToolUse deny (Edit requires `start_editing`).
Stop/AfterAgent hooks force `done_editing` before the agent can
finish responding. Per-agent scoping via `(session_id, agent_id)`.
SessionStart clears stale state. Supersedes the replace MCP tool
(workstream 11).

Two phases:
- **v1 (cold release):** Editing state table, MCP tools, PreToolUse
  deny, PostToolUse suppression, Stop/AfterAgent enforcement. No LSP
  traffic during editing. `done_editing` calls existing
  `DiagnosticsServer` unchanged.
- **v2 (warm state, after waitv2 1b):** `didOpen` on `start_editing`,
  `didChange` per edit, `done_editing` enters pipeline at `didSave`.
  Requires 1b infrastructure (composable pipeline,
  `TextDocumentSyncKind` on `ServerProfile`, `DocumentManager` as
  ref-counted lifecycle owner).

Constrained bash rewrite decoupled to workstream 14 (Recommend).
Replace removal is a separate cleanup ticket after v1 ships.

Design: `tickets/acquire/DESIGN.md`.
Issues: `tickets/acquire/ISSUES.md`.

---

## 14. Recommend

**Status: Design complete.** Ready for ticketing.

Replaces `scripts/constrained_bash.py` with a configurable Rust
implementation inside `catenary hook pre-tool`. Flips from allowlist
to denylist model with a `[recommendations]` config table — each
command maps to a disposition and guidance message. Three dispositions:
always deny, `when_first` (pipeline-safe), and `allow` (project
override). Template variables (`{read}`, `{edit}`, `{catenary_grep}`,
`{catenary_glob}`) resolve per-client at runtime. Project config
amends with `allow`/`deny` fields or overrides with `commands`.
Heredoc exception generalized to all denied commands. All parsing
behavior from the Python script preserved.

Independent of workstream 13 (Acquire). The existing Python script
works today; this ships on its own timeline.

Design: `tickets/recommend/DESIGN.md`.

---

## Contents

- `architecture/` — Host CLI and Catenary architecture references. Includes
  `lsp/spec-reference.md` (condensed LSP 3.17 spec for Catenary's subset).
- `decisions/` — Architectural decision records.
- `designs/` — Tool design documents (SEARCHv2, plus superseded GREP/SEARCH/GLOB).
- `tickets/` — Implementation tickets by workstream.
- `tools/` — Internal tooling and scripts.
- `archive/` — Historical session notes, superseded designs, and research.

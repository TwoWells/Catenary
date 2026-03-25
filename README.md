# Catenary

Internal planning and tracking for the Catenary project.

## Workstreams

| # | Workstream | Status | Tracker |
|---|-----------|--------|---------|
| 1 | [TUI rewrite](#1-tui-rewrite) | **Core complete** | `tickets/tui/README.md` |
| 2 | [MCP tool collapse](#2-mcp-tool-collapse) | **Complete** | `tickets/mcp/README.md` |
| 3 | [Lock removal + SQLite](#3-lock-removal--sqlite) | **Phase 2 complete** | `tickets/sql/README.md` |
| 4 | [SEARCHv2](#4-searchv2) | **In progress** | `tickets/searchv2/README.md` |
| 5 | [Misc](#5-misc) | **Open** | `tickets/misc/README.md` |
| 6 | [Wait model redesign](#6-wait-model-redesign) | **Reverted → v2 design** | `tickets/wait/DESIGN.md` |
| 7 | [Wait model v2](#7-wait-model-v2) | **1b ready** | `tickets/waitv2/README.md` |
| 8 | [Monitoring](#8-monitoring) | **Complete** | `tickets/monitoring/README.md` |
| 9 | [Filtering](#9-filtering) | **Design complete** | `tickets/filtering/DESIGN.md` |
| 10 | [Collapse](#10-collapse) | **Complete** | `tickets/collapse/README.md` |
| 11 | [Replace](#11-replace) | **Removed** | `tickets/replace/README.md` |
| 12 | [Summarize](#12-summarize) | **Complete** | `tickets/summarize/README.md` |
| 13 | [Diagnostic batching](#13-diagnostic-batching) | **v1 followup complete** | `tickets/acquire/DESIGN.md` |
| 14 | [Recommend](#14-recommend) | **Design complete** | `tickets/recommend/DESIGN.md` |
| 15 | [Management](#15-management) | **In progress** | `tickets/management/README.md` |

## Current priority

**1. Workstream 15 (Management).** 6 tickets (00–05). 00–01 done.
`FilesystemManager` (done), `LspClientManager` (done — absorbs
`DocumentManager`, `get_client(path)`, `Toolbox` as application
container, `McpRouter` rename), `inherit` config model, root
resolution, `HookRouter` extraction. Unblocks misc 28a.

**2. Workstream 7 (Wait model v2) 1b-00 (registration storage).**
Unblocks `didChangeConfiguration` dynamic registration for misc 28a.
Just 1b-00 — the rest of the 1b pipeline follows after misc 28a.

**3. Misc 28a (multi-root / workspace folders, tiers 1–2).**
Project-scoped servers, user-scoped workspace servers, per-root
instances for legacy servers, three-tier routing, `scopeUri`
resolution. Blocked on management 02, management 03, and 1b-00.
Subsumes misc 13. Misc 28b (single-file tier) follows after waitv2.

**4. Workstream 7 (Wait model v2) 1b-01+.** Remaining 1b pipeline
tickets (1b-01 through 1b-08, including 02a/02b split). Critical
path: 01 → 02a → 02b → 03 → {06, 07} → 08. Independent: 04
(OnceLock), 05 (dm utils). Full design:
`tickets/waitv2/design/pipeline_1b.md`.

**Lower priority:**

- **Workstream 13 (Acquire) v2 (warm state).** Blocked on waitv2
  1b infrastructure.
- **Workstream 4 (SEARCHv2) stale.** Ticket 00 complete. Design
  predates collapse (workstream 10), `ToolServer` trait, and
  async conversion (misc 30). Tickets 01+ need review against
  current architecture before resuming.
- **Workstream 14 (Recommend) design complete.** Python script
  works today; ship when needed.

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

**Status: Open.** Cross-cutting decisions and items.

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

**Status: Phase 1b ready.** All 8 Phase 0 tickets landed (0a, 0b,
0c, 0d1-0d4, 0e). Three-layer architecture (Connection, ServerInbox,
Client) in place, `lsp_types` removed, JSON builders/extractors
throughout. Phase 1a complete. Acquire v1 shipped — 1b is unblocked.

Phase 1b pipeline design finalized. 10 tickets (1b-00 through 1b-08,
including 02a/02b split). 1b tickets need updating to accommodate
acquire v2:
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

**Status: Complete.** All 6 phases done. Rearchitects the event system around Catenary's identity as a
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

**Status: Removed.** All 6 tickets (00a-00c, 01-03) landed and
shipped, then removed in acquire 03. `ReplaceServer`, MCP tool,
`snapshots` table, `catenary restore`, and sidecar logic deleted.

Was a batch edit MCP tool that accepted a glob pattern and a list
of `{old, new, flags}` edit operations. Applied all edits in one
tool call, created a snapshot before modification, returned
consolidated LSP diagnostics after all edits landed.

The tool solved the diagnostic firehose problem but required
voluntary adoption — agents never used it because they prefer the
host's Edit tool (trained behavior). Acquire/release (workstream 13)
solves the same problem by working with the agent's training:
the agent uses Edit, the hook system manages diagnostic timing.

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

**Status: v1 complete.** v2 blocked on waitv2 1b.

Diagnostic suppression via `start_editing`/`done_editing` MCP tools.
Stateless API — both tools take no file arguments. `start_editing`
enters editing mode; diagnostics are suppressed and modified file
paths accumulated by PostToolUse hooks. `done_editing` drains all
accumulated files and returns batch diagnostics. Forced adoption via
PreToolUse deny (Edit requires `start_editing`). Stop/AfterAgent
hooks force `done_editing` before the agent can finish responding.
Per-agent scoping via `(session_id, agent_id)`. SessionStart clears
stale state. Replace tool (workstream 11) removed in v1 followup
(acquire 03). Editing lifecycle methods live on `DocumentManager`
to support future waitv2 integration.

Two phases:
- **v1 (cold release) — complete.** Stateless editing mode flag,
  file accumulation in `editing_files` table, batch diagnostics via
  `DiagnosticsServer::process_files`. `EditingServer` removed;
  `Toolbox` holds `Arc<DiagnosticsServer>` shared between
  `LspBridgeHandler` and `HookServer`.
- **v2 (warm state, after waitv2 1b):** `didOpen` on `start_editing`,
  `didChange` per edit, `done_editing` enters pipeline at `didSave`.
  Requires 1b infrastructure (composable pipeline,
  `TextDocumentSyncKind` on `ServerProfile`, `DocumentManager` as
  ref-counted lifecycle owner).

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

## 15. Management

**Status: In progress.** 6 tickets (00–05). 00–01 done.

Rearchitects the file classification, client lookup, and application
container layers. `FilesystemCache` → `FilesystemManager` (done: single
authority for file metadata). `ClientManager` → `LspClientManager`
(done: absorbs `DocumentManager`, `get_client(path)`, `FilesystemManager`
injection). `Toolbox` as application container (done: creates all
internal dependencies, exposes `sync_roots`/`shutdown`).
`LspBridgeHandler` → `McpRouter` (done). Config keys unified with LSP
language IDs via `inherit`. `HookRouter` extraction (mirrors `McpRouter`
pattern). Cleanup pass deletes dead code paths.

Foundation for workstream 7 phase 1b and misc 28a (multi-root).

Design: `tickets/misc/29_language_id_config_unification.md`.
Tracker: `tickets/management/README.md`.

---

## Contents

- `dist/` — Public distribution submodule (`MarkWells-Dev/Catenary` on GitHub).
  Plugin manifests, hooks, docs site, install script, CI/CD, and public README.
- `architecture/` — Host CLI and Catenary architecture references. Includes
  `lsp/spec-reference.md` (condensed LSP 3.17 spec for Catenary's subset).
- `decisions/` — Architectural decision records.
- `designs/` — Tool design documents (SEARCHv2, plus superseded GREP/SEARCH/GLOB).
- `tickets/` — Implementation tickets by workstream.
- `tools/` — Internal tooling and scripts.
- `archive/` — Historical session notes, superseded designs, and research.

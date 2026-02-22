# LSP Fault Model

Catenary consumes output from third-party language servers that we do not maintain. LSP server responses must be treated as **unsanitized external input** — equivalent to user-supplied data crossing a trust boundary. A broken or malicious language server must never crash Catenary, corrupt user files, or produce errors that appear to originate from Catenary itself.

This document catalogs the failure modes, current handling, and required invariants.

---

## Principles

1. **Fault attribution.** Every error surfaced to the MCP client must clearly identify whether the failure is in the LSP server or in Catenary. The prefix `LSP error:` or the server language name should appear in all LSP-originated errors.

2. **Blast radius containment.** A failure in one language server must not affect other language servers, other workspace roots, or Catenary's MCP protocol handling.

3. **No silent degradation.** If a query returns partial results because a server is unavailable, the response must say so. "No symbols found" when the server is dead is a lie.

4. **Defense in depth on data.** URIs, positions, ranges, text content, and edit operations from the LSP are untrusted. Validate before use, especially before filesystem operations.

---

## Failure Categories

### 1. Process Failures

| Failure | Trigger | Current Handling | Status |
|---------|---------|-----------------|--------|
| Server won't start | Bad command, missing binary, permission error | `LspClient::spawn()` returns `Err`, propagated to `get_client()` | OK |
| Server crashes mid-session | Segfault, OOM, unhandled exception | Reader task detects stdout close, sets `alive=false`. Next request triggers restart via `get_client()` | OK |
| Server hangs (no response) | Deadlock, infinite loop | `REQUEST_TIMEOUT` (30s) fires, returns timeout error. Diagnostics wait uses activity tracking + nudge-and-retry — see [Timeout Ambiguity](#timeout-ambiguity-resolved) | OK |
| Server exits during initialize | Crash on startup | `initialize()` request times out or gets channel-closed error | OK |
| Server produces no stdout | Blocks on stderr, misconfigured pipes | Timeout on first request | OK |

### 2. Protocol Failures

| Failure | Trigger | Current Handling | Status |
|---------|---------|-----------------|--------|
| Malformed JSON | Truncated output, encoding bugs | `serde_json::from_str` fails in reader task, logged as warn, **message silently skipped** | **Problem** — see [Orphaned Requests](#orphaned-requests) |
| Invalid Content-Length | Off-by-one, missing header | `try_parse_message()` waits for more data or returns parse error | OK |
| Response without matching ID | Server bug, ID reuse | Logged as warn, response discarded | OK |
| Notification with unknown method | Server extensions, custom notifications | Logged as trace, ignored | OK |
| Server request (e.g. workspace/configuration) | Normal LSP behavior | Replied with MethodNotFound (-32601) | OK |
| Wrong JSON-RPC version | Non-compliant server | Serde deserializes `jsonrpc` field but doesn't validate value | Low risk |

### 3. Response Data Failures

| Failure | Trigger | Current Handling | Status |
|---------|---------|-----------------|--------|
| Wrong response type | Server returns string where object expected | `serde_json::from_value` fails, returns error prefixed with `[language]` | OK |
| Null where value expected | Server omits required field | Depends on `Option` wrapping in lsp-types. Serde handles most cases. | OK for optional fields |
| Empty results | Server has no data | Returns "No hover information" etc. | OK |
| Extremely large response | Server dumps entire AST | No size limit on response parsing | **Problem** — see [Unbounded Data](#unbounded-data) |
| Invalid URI in response | Mangled paths, non-file:// schemes | `uri.path()` used directly without validation | **Problem** — see [URI Trust](#uri-trust) |
| Out-of-range positions | Line/column beyond file bounds | Edits returned as text, MCP client applies | OK |
| Wrong position encoding | Server claims UTF-8 but sends UTF-16 offsets | Encoding taken from initialize response, no runtime validation | **Problem** — see [Encoding Trust](#encoding-trust) |
| Stale diagnostic data | Server sends diagnostics for old file version | Cached and served as current | Low risk — diagnostics are advisory |

### 4. Workspace Edit Failures

LSP servers propose workspace edits (via rename, code actions, formatting). These edits contain URIs, byte ranges, and replacement text — all untrusted.

**Design decision:** Catenary does not apply workspace edits to the filesystem. LSP tools (`rename`, `apply_quickfix`, `formatting`) return proposed edits as structured text. The MCP client reviews and applies them using its own editing tools, or via Catenary's `edit_file` tool which validates paths against workspace roots.

This eliminates an entire class of failures:

| Failure | Trigger | Resolution |
|---------|---------|------------|
| Edit targets file outside workspace | Path traversal in URI | MCP client controls file writes, not the LSP |
| Overlapping edit ranges | Server bug | MCP client applies edits individually with full file context |
| Edit with wrong encoding offsets | Encoding mismatch | MCP client works with text, not byte offsets |
| ResourceOp (create/rename/delete) | Code action side effects | Surfaced as proposed operations; MCP client decides |

**Rationale:** The MCP clients calling Catenary (Claude Code, Gemini CLI, etc.) already have file editing tools with their own safety checks. Having Catenary also write files creates a redundant, less-validated write path that trusts LSP-provided URIs and byte offsets. Removing it enforces a clean trust boundary: LSP servers propose, the MCP client disposes.

Catenary's `edit_file` and `write_file` tools validate all paths against workspace roots and return post-edit diagnostics. The MCP client can use `edit_file` to apply LSP-proposed changes, keeping the trust boundary intact — the LSP still never gets direct write access.

### 5. Multi-Root Specific Failures

| Failure | Trigger | Current Handling | Status |
|---------|---------|-----------------|--------|
| Server handles one root, ignores others | Server doesn't support multi-root workspaces | Server initialized with all roots, but behavior is server-dependent | Acceptable — can't fix broken servers |
| `didChangeWorkspaceFolders` rejected | Server doesn't support dynamic workspace changes | Error logged as warn, other servers unaffected | OK |
| Cross-root references | Symbol in root A references file in root B | Works if server supports it; fails gracefully if not | OK |
| Partial workspace search results | One server dead during workspace search | Warning appended to response: `"Warning: [lang] unavailable, results may be incomplete"` | OK |

---

## Open Issues

### Orphaned Requests

**Location:** `src/lsp/client.rs` reader task, line ~190

When the reader task encounters malformed JSON, it logs a warning and skips the message. If that message was a response to a pending request, the request stays in the `pending` map and blocks until `REQUEST_TIMEOUT` (30s). The eventual timeout error says "timed out" — it doesn't mention that the server sent garbage.

**Impact:** 30-second hang followed by a misleading error message.

**Fix:** When skipping a malformed message, attempt to extract the `id` field from the raw string (even if full deserialization failed) and fail the pending request with a clear "server sent malformed response" error.

### ~~Error Attribution~~ (Resolved)

All LSP-originated errors are now prefixed with `[language]`, e.g., `[rust] request timed out` or `[python] server closed connection`. The `LspClient` stores its language identifier and includes it in all error messages from the `request()` method. Handler-level errors (e.g., "server is no longer running") also include the language prefix.

### ~~Timeout Ambiguity~~ (Resolved)

`wait_for_diagnostics_update` returns a two-variant enum (`DiagnosticsWaitResult`): `Updated` or `ServerDied`. Each LSP server is assigned a `DiagnosticsStrategy` based on runtime observations:

- **Version** — server includes `version` in `publishDiagnostics`. Wait for generation advance.
- **`TokenMonitor`** — server sends `$/progress` tokens. Wait for Active -> Idle cycle with a hard timeout.
- **`ProcessMonitor`** — no progress tokens, no version. Poll CPU time via `/proc/<pid>/stat` (Linux) or `ps` (macOS). Trust-based patience decays on consecutive timeouts without diagnostics.

All strategies include a Phase 2 settle: 2 seconds of silence with no active progress tokens, catching servers that publish diagnostics in multiple rounds. Callers send `didSave` unconditionally after every change (handling servers that only run diagnostics on save) and make a single `wait_for_diagnostics_update` call — no retry loop.

### URI Trust

**Location:** Multiple points in `src/bridge/handler.rs` — `format_definition_response`, `find_symbol_in_workspace_response`, `format_locations_with_definition`, etc.

`uri.path()` is extracted from LSP responses and converted to `PathBuf` without validation. A buggy server could return URIs like `file:///etc/passwd` or `file:///workspace/../../../etc/shadow`.

For **read-only operations** (hover, definition, references): the URI is used for display only. Risk is low — it shows a misleading path but doesn't access the file.

**Write operations are not affected.** Catenary does not apply workspace edits directly (see [Workspace Edit Failures](#4-workspace-edit-failures)). LSP-provided URIs in edits are passed through as text for the MCP client to evaluate.

### Unbounded Data

**Location:** Throughout response handling

There are no size limits on:
- Diagnostic arrays (cached per URI, never evicted except on new publish)
- Completion response arrays (capped at 50 items in formatting — good)
- Hover content length
- Workspace symbol results
- Document symbol tree depth (recursive traversal)

**Fix for diagnostics:** Cap diagnostics per URI. Evict entries for URIs that haven't been queried recently.

**Fix for recursive traversal:** Add depth limit to `format_nested_symbols()` and related recursive functions.

### ~~Silent Partial Results~~ (Resolved)

`search` always runs both LSP workspace symbols and a ripgrep file heatmap. If an LSP server is unavailable, its symbols are silently omitted — the heatmap covers the gap. `codebase_map` appends `"Warning: [lang] unavailable, symbols may be incomplete"` when a server fails during symbol collection.

### Signature Help Label Offsets

**Location:** `src/bridge/handler.rs` `format_signature_help()`, line ~2621

`ParameterLabel::LabelOffsets([start, end])` is used for substring extraction via `.skip(start).take(end - start)` on a char iterator. If offsets are invalid (beyond string length, or `end < start`), the result is silently truncated or empty rather than producing an error.

**Impact:** Low — display-only, no data corruption. But could produce confusing output.

---

## Invariants

These properties must hold regardless of LSP server behavior:

1. **Catenary never crashes** due to LSP server output. All deserialization is fallible. All `unwrap()` on LSP data is forbidden.

2. **Catenary never modifies the filesystem based on LSP data.** LSP-proposed edits (rename, code actions, formatting) are returned as structured text. Catenary's `edit_file` and `write_file` tools validate all paths against workspace roots independently of LSP data — the LSP never gets direct write access.

3. **Catenary never hangs indefinitely.** All LSP requests have bounded timeouts. Diagnostics waits use activity-based tracking with nudge-and-retry (bounded by attempt count). Reader task failures don't block the MCP server.

4. **Error messages identify the source.** LSP-originated errors include the server language/name. Catenary errors don't mention LSP.

5. **Partial results are labeled.** If a query couldn't reach all configured servers, the response indicates this.

6. **One server's failure doesn't affect others.** Each language server is independent. A crash in rust-analyzer doesn't break pylsp.

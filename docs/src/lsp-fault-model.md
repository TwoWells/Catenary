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
| Server hangs (no response) | Deadlock, infinite loop | `REQUEST_TIMEOUT` (30s) fires, returns timeout error | Partial — see [Timeout Ambiguity](#timeout-ambiguity) |
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
| Wrong response type | Server returns string where object expected | `serde_json::from_value` fails, returns `Err("Failed to parse LSP response")` | **Problem** — see [Error Attribution](#error-attribution) |
| Null where value expected | Server omits required field | Depends on `Option` wrapping in lsp-types. Serde handles most cases. | OK for optional fields |
| Empty results | Server has no data | Returns "No hover information" etc. | OK |
| Extremely large response | Server dumps entire AST | No size limit on response parsing | **Problem** — see [Unbounded Data](#unbounded-data) |
| Invalid URI in response | Mangled paths, non-file:// schemes | `uri.path()` used directly without validation | **Problem** — see [URI Trust](#uri-trust) |
| Out-of-range positions | Line/column beyond file bounds | `position_to_offset()` returns error on out-of-bounds | OK |
| Wrong position encoding | Server claims UTF-8 but sends UTF-16 offsets | Encoding taken from initialize response, no runtime validation | **Problem** — see [Encoding Trust](#encoding-trust) |
| Stale diagnostic data | Server sends diagnostics for old file version | Cached and served as current | Low risk — diagnostics are advisory |

### 4. Workspace Edit Failures

LSP servers propose workspace edits (via rename, code actions, formatting). These edits contain URIs, byte ranges, and replacement text — all untrusted.

**Design decision:** Catenary does not apply workspace edits to the filesystem. LSP tools (`rename`, `apply_quickfix`, `formatting`) return proposed edits as structured text. The MCP client reviews and applies them using its own editing tools, or (in Phase 7) via Catenary's `edit_file` tool which validates paths against workspace roots.

This eliminates an entire class of failures:

| Failure | Trigger | Resolution |
|---------|---------|------------|
| Edit targets file outside workspace | Path traversal in URI | MCP client controls file writes, not the LSP |
| Overlapping edit ranges | Server bug | MCP client applies edits individually with full file context |
| Edit with wrong encoding offsets | Encoding mismatch | MCP client works with text, not byte offsets |
| ResourceOp (create/rename/delete) | Code action side effects | Surfaced as proposed operations; MCP client decides |

**Rationale:** The MCP clients calling Catenary (Claude Code, Gemini CLI, etc.) already have file editing tools with their own safety checks. Having Catenary also write files creates a redundant, less-validated write path that trusts LSP-provided URIs and byte offsets. Removing it enforces a clean trust boundary: LSP servers propose, the MCP client disposes.

**Phase 7 connection:** When Catenary adds `edit_file` and `write_file` tools, those tools will validate all paths against workspace roots and return post-edit diagnostics. The MCP client can use `edit_file` to apply LSP-proposed changes, keeping the trust boundary intact — the LSP still never gets direct write access.

### 5. Multi-Root Specific Failures

| Failure | Trigger | Current Handling | Status |
|---------|---------|-----------------|--------|
| Server handles one root, ignores others | Server doesn't support multi-root workspaces | Server initialized with all roots, but behavior is server-dependent | Acceptable — can't fix broken servers |
| `didChangeWorkspaceFolders` rejected | Server doesn't support dynamic workspace changes | Error logged as warn, other servers unaffected | OK |
| Cross-root references | Symbol in root A references file in root B | Works if server supports it; fails gracefully if not | OK |
| Partial workspace search results | One server dead during `find_symbol_in_workspace` iteration | **Results from dead server silently omitted** | **Problem** — see [Silent Partial Results](#silent-partial-results) |

---

## Open Issues

### Orphaned Requests

**Location:** `src/lsp/client.rs` reader task, line ~190

When the reader task encounters malformed JSON, it logs a warning and skips the message. If that message was a response to a pending request, the request stays in the `pending` map and blocks until `REQUEST_TIMEOUT` (30s). The eventual timeout error says "timed out" — it doesn't mention that the server sent garbage.

**Impact:** 30-second hang followed by a misleading error message.

**Fix:** When skipping a malformed message, attempt to extract the `id` field from the raw string (even if full deserialization failed) and fail the pending request with a clear "server sent malformed response" error.

### Error Attribution

**Location:** `src/mcp/server.rs` line ~260, `src/lsp/client.rs` line ~397

LSP errors are wrapped in `CallToolResult::error()` with messages like `"Failed to parse LSP response"` or `"LSP request 'textDocument/hover' timed out"`. These appear in the MCP response as `isError: true` but are indistinguishable from Catenary bugs without parsing the error text.

**Fix:** Prefix all LSP-originated errors consistently: `"[rust-analyzer] request timed out"` or `"[pylsp] invalid response for textDocument/hover"`. Include the language server name so the user knows which server to investigate.

### Timeout Ambiguity

**Location:** `src/lsp/client.rs` line ~375

A 30-second timeout doesn't distinguish between "server is slow but working" and "server is hung." After timeout, the next request may succeed (slow server) or also timeout (hung server).

**Possible improvement:** Track consecutive timeouts per client. After N consecutive timeouts, mark server as unhealthy and include this in error messages: `"[rust-analyzer] timed out (3 consecutive failures, server may be hung)"`.

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

### Silent Partial Results

**Location:** `src/bridge/handler.rs` `find_symbol_in_workspace()`, line ~660

When iterating active clients for workspace symbol search, a dead client is silently skipped via `if let Ok(Some(...))`. The MCP client receives results from surviving servers with no indication that coverage is incomplete.

**Fix:** Track which clients were queried and which failed. Append a note to the response: `"Note: rust-analyzer is not responding, results may be incomplete"`.

### Signature Help Label Offsets

**Location:** `src/bridge/handler.rs` `format_signature_help()`, line ~2621

`ParameterLabel::LabelOffsets([start, end])` is used for substring extraction via `.skip(start).take(end - start)` on a char iterator. If offsets are invalid (beyond string length, or `end < start`), the result is silently truncated or empty rather than producing an error.

**Impact:** Low — display-only, no data corruption. But could produce confusing output.

---

## Invariants

These properties must hold regardless of LSP server behavior:

1. **Catenary never crashes** due to LSP server output. All deserialization is fallible. All `unwrap()` on LSP data is forbidden.

2. **Catenary never modifies the filesystem based on LSP data.** LSP-proposed edits (rename, code actions, formatting) are returned as structured text. File writes are the MCP client's responsibility. When Phase 7 adds `edit_file`, it validates all paths against workspace roots independently of LSP data.

3. **Catenary never hangs indefinitely.** All LSP requests have bounded timeouts. Reader task failures don't block the MCP server.

4. **Error messages identify the source.** LSP-originated errors include the server language/name. Catenary errors don't mention LSP.

5. **Partial results are labeled.** If a query couldn't reach all configured servers, the response indicates this.

6. **One server's failure doesn't affect others.** Each language server is independent. A crash in rust-analyzer doesn't break pylsp.

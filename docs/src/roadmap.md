# Roadmap

Current version: **v0.6.6**

## Completed

### catenary-mcp (v0.6.x) — MCP Bridge ✓

LSP tools exposed via MCP. Feature complete.

<details>
<summary>Development History</summary>

#### Phase 1: Configuration Logic

- [x] Add `config` and `dirs` dependencies
- [x] Define `Config` struct (using `serde`)
- [x] Implement config loading from `XDG_CONFIG_HOME` or `--config` flag

#### Phase 2: Lazy Architecture

- [x] Create `ClientManager` struct
- [x] Move `spawn` and `initialize` logic from `main.rs` into
      `ClientManager::get_or_spawn`
- [x] Update `LspBridgeHandler` to use `ClientManager`

#### Phase 3: Cleanup & Optimization

- [x] Update `document_cleanup_task` to communicate with `ClientManager`
- [x] Implement server shutdown logic when no documents are open for that
      language

#### Phase 4: Context Awareness ("Smart Wait")

- [x] **Progress Tracking:** Monitor LSP `$/progress` notifications to detect
      "Indexing" states
- [x] **Smart Blocking:** Block/Queue requests while the server is initializing
      or indexing
- [x] **Internal Retry:** Retry internally if a server returns `null` shortly
      after spawn
- [x] **Status Tool:** Add `status` tool to report server states

#### Phase 4.5: Observability & CD

- [x] **Session Monitoring:** Add `catenary list` and `catenary monitor`
      commands
- [x] **Event Broadcasting:** Broadcast tool calls, results, and raw MCP
      messages
- [x] **CI/CD:** Add GitHub Actions for automated testing, release builds, and
      crates.io publishing

#### Phase 5: High-Level Tools ("Catenary Intelligence")

- [x] **Auto-Fix:** Add `apply_quickfix` tool (chains `codeAction` +
      `workspaceEdit` application)
- [x] **Codebase Map:** Add `codebase_map` to generate a high-level
      semantic tree of the project (synthesized from file walk +
      `documentSymbol`)
- [x] **Relative Path Support:** Resolve relative paths in tool arguments
      against the current working directory

</details>

### CLI Integration Research ✓

Validated approach: use existing CLI tools (Claude Code, Gemini CLI) with
built-in tools disabled, replaced by catenary-mcp.

<details>
<summary>Findings</summary>

**Why not a custom CLI?** Subscription plans ($20/month Pro tier) are tied to
official CLI tools. A custom CLI requires pay-per-token API access — wrong
billing model for individual developers.

**Validated configurations:**

- **Gemini CLI:** `tools.core` allowlist (blocklist doesn't work)
- **Claude Code:** `permissions.deny` + must block `Task` to prevent sub-agent
  escape

See [CLI Integration](cli-integration.md) for full details.

</details>

---

## Known Vulnerabilities

These must be resolved before v1.0.0. See [LSP Fault Model](lsp-fault-model.md)
and [Adversarial Testing](adversarial-testing.md) for full details.

- **Symlink traversal.** `codebase_map` and `find_symbol` fallback use
  `WalkBuilder` which follows symlinks. A symlink pointing outside the
  workspace allows reading files the user didn't intend to expose.
- **Unbounded LSP data.** Diagnostic caches grow without limit. Hover
  responses, symbol trees, and workspace edit previews have no size caps.
  A malicious or buggy LSP server can cause unbounded memory growth.
- **`apply_workspace_edit` trusts LSP URIs.** Workspace edits can target
  files outside the workspace. Tracked for removal in Phase 6.5.

---

## In Progress

### Phase 6: Multi-Workspace Support

Single Catenary instance multiplexing across multiple workspace roots.

- [x] Accept multiple `--root` paths
- [x] Pass all roots as `workspace_folders` to each LSP server
- [x] Multi-root `find_symbol` fallback (ripgrep + manual search across roots)
- [x] Multi-root `codebase_map` (walks all roots, prefixes entries in multi-root mode)
- [x] `add_root()` plumbing (appends root, sends `didChangeWorkspaceFolders`)
- [x] Expose `add_root` mid-session via MCP `roots/list`

### Phase 6.5: Hardening

- [ ] Remove `apply_workspace_edit` — `rename`, `apply_quickfix`, and
      `formatting` return proposed edits only; MCP client applies them
      (see [LSP Fault Model](lsp-fault-model.md#4-workspace-edit-failures)).
      **Open question:** return edits as human-readable text (current
      dry-run format) or structured JSON for mechanical application?
- [ ] Error attribution — prefix all LSP-originated errors with server
      name: `[rust-analyzer] request timed out`
- [ ] Silent partial results — warn when workspace search skips a dead
      server
- [ ] Pass `initializationOptions` from config to LSP server
- [ ] `search` — unified search tool (LSP → grep fallback), with clear
      messaging when using fallback ("grep cannot distinguish definition
      from usage")
- [ ] Update documentation

### Phase 7: Complete Agent Toolkit — v1.0.0

Full toolset to replace CLI built-in tools. Requires CLI settings changes
(Claude Code, Gemini CLI) to disable built-in tools in favor of Catenary.

**File I/O:**
- [ ] `read_file` — Read file contents + return diagnostics
- [ ] `write_file` — Write file + return diagnostics
- [ ] `edit_file` — Edit file + return diagnostics
- [ ] `list_directory` — List directory contents

**Shell Execution:**
- [ ] `run` tool with allowlist enforcement
- [ ] `allowed = ["*"]` opt-in for unrestricted shell
- [ ] Dynamic language detection — language-specific commands activate when
      matching files exist in the workspace
- [ ] Tool description updates dynamically to show current allowlist:
      `"Allowed: git, make. Python (detected): python, pytest, uv"`
- [ ] Emit `tools/list_changed` when allowlist changes (e.g., workspace added)
- [ ] Error messages on denied commands include the current allowlist

```toml
[tools.run]
allowed = ["git", "make"]

[tools.run.python]
allowed = ["python", "pytest", "uv"]

[tools.run.rust]
allowed = ["cargo"]
```

**Design constraints:**
- Write/edit tools return LSP diagnostics automatically. Models can't proceed
  unaware they broke something.
- Write/edit tools validate all paths against workspace roots. LSP-proposed
  edits (rename, code actions) are applied through these tools, never directly
  from LSP data (see [LSP Fault Model](lsp-fault-model.md)).
- Write/edit tools refuse to modify Catenary's own config files. An agent that
  can rewrite its own permissions isn't a controlled agent.

---

## Backlog

- [ ] **Semantic Search:** Integrate local embeddings (RAG) for fuzzy code
      search (e.g., "Find auth logic")
- [ ] **Batch Operations:** Query hover/definition/references for multiple
      positions in a single call
- [ ] **References with Context:** Include surrounding lines (e.g., `-C 3`) in
      reference results
- [ ] **Multi-file Diagnostics:** Check diagnostics across multiple files in
      one call

---

## Abandoned

### catenary-cli — Custom Agent Runtime

Originally planned to build a custom CLI to control the model agent loop.
Abandoned because subscription plans are tied to official CLI tools.

See [Archive: CLI Design](archive/cli-design.md) for the original design.

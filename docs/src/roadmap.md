# Roadmap

Current version: **v0.6.1**

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

## In Progress

### Phase 6: File I/O Tools

Add file operations to catenary-mcp so models can complete coding tasks without
built-in tools.

- [ ] `catenary_read_file` — Read file contents
- [ ] `catenary_write_file` — Write file + return diagnostics
- [ ] `catenary_edit_file` — Edit file + return diagnostics
- [ ] `catenary_list_directory` — List directory contents

**Design constraint:** Write/edit tools return LSP diagnostics automatically.
Models can't proceed unaware they broke something.

### Phase 6.5: Polish

- [ ] Pass `initializationOptions` from config to LSP server
- [ ] Support `DocumentChange` operations (create/rename/delete) in
      `apply_workspace_edit`
- [ ] Update documentation

---

## Next

### Phase 7: Grep Fallback

When LSP unavailable, fall back to grep with degradation notice.

- [ ] `catenary_search` — unified search tool (LSP → grep fallback)
- [ ] Clear messaging when using fallback ("grep cannot distinguish definition
      from usage")

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

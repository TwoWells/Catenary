# Catenary Roadmap: Towards v0.3.0 ("Smart Catenary")

The goal of v0.3.0 is to transform Catenary from a simple multiplexer into a resource-efficient, configurable, and intelligent LSP hub.

## 🎯 Core Objectives

1.  **Smart Resource Management:** Only run language servers when they are needed (Lazy Loading).
2.  **Configuration System:** Replace unwieldy CLI arguments with a structured configuration file.
3.  **Advanced Control:** Support `initializationOptions` and per-server settings.

## 🏗️ Architecture Changes

### 1. Configuration (`~/.config/catenary/config.toml`)
Users should be able to define servers declaratively.

```toml
# Global settings
idle_timeout = 300

[server.rust]
command = "rust-analyzer"
args = []
initialization_options = { checkOnSave = { command = "clippy" } }

[server.python]
command = "pyright-langserver"
args = ["--stdio"]

[server.bash]
command = "bash-language-server"
args = ["start"]
```

### 2. ClientManager (The "Brain")
Refactor `LspBridgeHandler` to use a dynamic `ClientManager` instead of a static `HashMap`.

- **State:** Stores `ServerConfig` (definitions) and `ActiveClients` (running instances).
- **Get(lang_id):**
    - Checks if client is running.
    - If yes -> Returns it.
    - If no -> Spawns it, initializes it, adds to active list, returns it.
- **Broadcast:** Iterates only over *active* clients for workspace queries.

### 3. Server Lifecycle
- **Startup:** Instant. No servers spawned.
- **Shutdown:** Automatic shutdown of individual servers after idle timeout (extending existing doc cleanup).

## 📝 Implementation Plan

### Phase 1: Configuration Logic
- [x] Add `config` and `dirs` dependencies.
- [x] Define `Config` struct (using `serde`).
- [x] Implement config loading from `XDG_CONFIG_HOME` or `--config` flag.

### Phase 2: Lazy Architecture
- [x] Create `ClientManager` struct.
- [x] Move `spawn` and `initialize` logic from `main.rs` into `ClientManager::get_or_spawn`.
- [x] Update `LspBridgeHandler` to use `ClientManager`.

### Phase 3: Cleanup & Optimization
- [x] Update `document_cleanup_task` to communicate with `ClientManager`.
- [x] Implement server shutdown logic when no documents are open for that language.

### Phase 4: Context Awareness ("Smart Wait")
The goal is to bridge the gap between stateless Agents and stateful LSPs. Agents prefer latency over incorrect "No info" responses.

- [ ] **Progress Tracking:** Monitor LSP `$/progress` notifications to detect "Indexing" states.
- [ ] **Smart Blocking:** Block/Queue requests while the server is initializing or indexing (instead of failing fast).
- [ ] **Internal Retry:** If a server returns `null` shortly after spawn, wait and retry internally before returning empty results to the agent.
- [ ] **Status Tool:** Add `catenary_status` tool to report server states (e.g., "rust: Indexing", "bash: Ready").

### Phase 5: Cleanup & Refinement
- [ ] **Auto-Fix:** Add `lsp_apply_code_action` tool to apply Quick Fixes (using the existing edit application logic).
- [ ] Pass `initializationOptions` from config to LSP server (`src/lsp/manager.rs`).
- [ ] Support `DocumentChange` operations (create/rename/delete) in `apply_workspace_edit` (`src/bridge/handler.rs`).
- [ ] Update documentation.

## 🚀 v1.0.0 Future Ideas
- [ ] **Semantic Search:** Integrate local embeddings (RAG) for "fuzzy" code search (e.g., "Find auth logic").

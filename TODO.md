# Add Root Mid-Session via MCP `roots/list`

Enable Catenary to discover and react to workspace root changes from the MCP
client at runtime, completing the Phase 6 multi-workspace feature.

## Tasks

### 1. Bidirectional JSON-RPC in `McpServer`

Currently `McpServer` only responds to client requests. It cannot initiate
requests to the client, which `roots/list` requires (server-to-client request).

- [ ] Add a channel (or writer handle) to `McpServer` for sending outbound
      requests to the client's stdin
- [ ] Implement `send_request(&self, method, params) -> Result<Value>` that
      writes a JSON-RPC request, assigns an ID, and awaits the response
- [ ] Route incoming messages: responses (with matching ID) go to the pending
      request future; requests/notifications go to existing `handle_message`
- [ ] **Unit test:** round-trip a server-initiated request through a mock
      client (in-memory channel pair)

### 2. Store client capabilities from `initialize`

- [ ] Save `ClientCapabilities` (specifically `roots.list_changed`) from the
      `InitializeParams` during `handle_initialize`
- [ ] Expose a method to check whether the client supports `roots/list_changed`
- [ ] **Unit test:** verify capabilities are stored when present and absent

### 3. Send `roots/list` after initialization

- [ ] After receiving `notifications/initialized`, send a `roots/list` request
      to the client
- [ ] Parse the response as `Vec<Root>` (add `Root` type to `mcp/types.rs`:
      `{ uri: String, name?: String }`)
- [ ] Call `ClientManager::add_root` for each root not already present
- [ ] **Unit test:** mock client returns roots, verify `ClientManager` state
      updates
- [ ] **Integration test:** full MCP session where the client provides roots
      via `roots/list` and verify LSP servers receive the workspace folders

### 4. Handle `notifications/roots/list_changed`

- [ ] Add a match arm in `handle_notification` for
      `"notifications/roots/list_changed"`
- [ ] On receipt, send a new `roots/list` request to get the updated list
- [ ] Diff against current roots: add new ones, remove stale ones
- [ ] **Unit test:** simulate notification, verify diff logic (adds and
      removes)

### 5. Implement `ClientManager::remove_root`

- [ ] Add `remove_root(&self, root: PathBuf)` that removes the path from
      `self.roots` and sends `didChangeWorkspaceFolders` with the root in the
      `removed` list
- [ ] **Unit test:** mirror `test_add_root_appends` — verify root is removed
      and list shrinks
- [ ] **Integration test:** remove a root mid-session and verify the LSP
      server receives `didChangeWorkspaceFolders` with the correct removed
      folder

### 6. Guard on client capabilities

- [ ] Only send `roots/list` and subscribe to `roots/list_changed` if
      `capabilities.roots.list_changed == true`
- [ ] If the client doesn't advertise roots support, log a debug message and
      skip — current CLI-arg-only behavior is preserved
- [ ] **Unit test:** verify no `roots/list` request is sent when capability
      is absent

### 7. Update roadmap

- [ ] Mark "Expose `add_root` mid-session" as complete in `docs/src/roadmap.md`

# mockls

mockls is a configurable mock LSP server built into Catenary's test suite. It speaks the LSP protocol over stdin/stdout but lets CLI flags control its capabilities, timing, and failure modes. Tests compose flags to simulate specific server behaviors without depending on real language servers.

## Motivation

Catenary's integration tests originally depended on real language servers (bash-language-server, rust-analyzer, taplo). This caused three problems:

1. **Upstream coupling.** Tests asserted on upstream behavior that could change at any time. A bash-lsp update could break Catenary's test suite without any Catenary code changing.

2. **Non-reproducible CI.** Tests skipped when servers weren't installed. Different machines ran different subsets of the suite.

3. **No adversarial coverage.** Real servers behave well. There was no way to test how Catenary handles slow indexing, dropped connections, flaky responses, or hung servers.

mockls solves all three: it provides a fixed target with composable behavioral axes. Bugs reported against real servers get reproduced as mockls flag combinations and stay in the suite forever.

## Design

mockls is a synchronous binary (`src/bin/mockls.rs`). No tokio — it uses `std::thread` for deferred notifications (diagnostics delays, indexing simulation). Messages are Content-Length framed JSON-RPC, the same wire format as real LSP servers.

The server stores document content in memory on `didOpen`/`didChange` and provides minimal text-based intelligence: word extraction for hover, pattern matching for definitions, string search for references, and keyword scanning for symbols. This is enough to exercise all of Catenary's LSP client code paths without implementing real language analysis.

## CLI Flags

Flags are composable behavioral axes, not named presets.

| Flag | Default | Effect |
|---|---|---|
| `--workspace-folders` | off | Advertise `workspaceFolders` capability with `changeNotifications` |
| `--indexing-delay <ms>` | 0 | Emit `window/workDoneProgress/create` + `$/progress` begin/end after `initialized` |
| `--response-delay <ms>` | 0 | Sleep before every response |
| `--diagnostics-delay <ms>` | 0 | Delay before publishing diagnostics |
| `--no-diagnostics` | off | Never publish diagnostics |
| `--diagnostics-on-save` | off | Only publish diagnostics on `didSave`, not `didOpen`/`didChange` |
| `--drop-after <n>` | none | Close stdout after n responses (simulate crash) |
| `--hang-on <method>` | none | Never respond to this method (repeatable) |
| `--fail-on <method>` | none | Return `InternalError` (-32603) for this method (repeatable) |
| `--send-configuration-request` | off | Send `workspace/configuration` request after initialize |

### Example profiles

A "rust-analyzer-like" test:
```
mockls --workspace-folders --indexing-delay 3000 --diagnostics-on-save --send-configuration-request
```

A "bash-lsp-like" test (no flags — the default):
```
mockls
```

A crash reproduction:
```
mockls --drop-after 3
```

A server that hangs on hover:
```
mockls --hang-on textDocument/hover
```

The flags document exactly what behavior each test targets.

## LSP Methods

### Requests (respond with result or error)

| Method | Behavior |
|---|---|
| `initialize` | Returns capabilities based on flags |
| `shutdown` | Returns null |
| `textDocument/hover` | Extracts word at position, returns as markdown code block |
| `textDocument/definition` | Scans for definition pattern (`fn`, `function`, `def`, `let`, `const`, `var`); falls back to first occurrence |
| `textDocument/references` | Returns all positions where the word appears in the document |
| `textDocument/documentSymbol` | Scans for lines matching keyword patterns, returns `DocumentSymbol` array |
| `workspace/symbol` | Searches across all stored documents |

### Notifications (no response)

| Method | Behavior |
|---|---|
| `initialized` | Starts indexing simulation if `--indexing-delay` is set |
| `textDocument/didOpen` | Stores content, publishes diagnostics (unless suppressed) |
| `textDocument/didChange` | Updates content, republishes diagnostics (unless suppressed) |
| `textDocument/didSave` | Publishes diagnostics (unless `--no-diagnostics`) |
| `textDocument/didClose` | Removes document from store |
| `workspace/didChangeWorkspaceFolders` | Accepted silently |
| `exit` | Exits the process |

### Server-to-client messages

| Message | When |
|---|---|
| `textDocument/publishDiagnostics` | One warning per document on line 0: "mockls: mock diagnostic" |
| `window/workDoneProgress/create` | Before indexing simulation |
| `$/progress` (begin/end) | During indexing simulation |
| `workspace/configuration` | If `--send-configuration-request` is set |

## Diagnostics Trigger Behavior

mockls never publishes diagnostics spontaneously at startup — only in response to document events. This models the pattern where `has_published_diagnostics` stays false during warmup.

| Config | didOpen | didChange | didSave |
|---|---|---|---|
| Default | publishes | publishes | publishes |
| `--diagnostics-on-save` | no | no | publishes |
| `--no-diagnostics` | no | no | no |
| `--diagnostics-delay <ms>` | publishes after delay | publishes after delay | publishes after delay |

These map to specific code paths in Catenary's `wait_for_diagnostics_update`:

- **Default:** Server publishes promptly on `didOpen`, exercises Phase 1 generation advance.
- **`--diagnostics-on-save`:** Server ignores `didOpen`/`didChange`, goes silent. Catenary detects inactivity, re-sends `didSave`, mockls publishes on the nudge.
- **`--no-diagnostics`:** Exercises the "never published" grace period timeout path. Catenary handles servers that never emit diagnostics without hanging.
- **`--diagnostics-delay`:** Diagnostics arrive late, exercises Phase 1 activity tracking.

## Usage in Tests

### Integration tests (`tests/mcp_integration.rs`)

The `mockls_lsp_arg` helper builds `--lsp` arguments for `BridgeProcess::spawn`:

```rust
fn mockls_lsp_arg(lang: &str, flags: &str) -> String {
    let bin = env!("CARGO_BIN_EXE_mockls");
    if flags.is_empty() {
        format!("{lang}:{bin}")
    } else {
        format!("{lang}:{bin} {flags}")
    }
}
```

Tests iterate over profiles — same test logic, different mockls behavior each iteration:

```rust
let profiles: &[(&str, &str)] = &[
    ("clean", ""),
    ("workspace-folders", "--workspace-folders"),
];

for (name, flags) in profiles {
    let lsp = mockls_lsp_arg("shellscript", flags);
    let mut bridge = BridgeProcess::spawn(&[&lsp], "/tmp")?;
    // ... test logic ...
}
```

### Unit tests in manager (`src/lsp/manager.rs`)

The `mockls_config()` and `mockls_workspace_folders_config()` helpers create `Config` structs that point to the mockls binary. This replaced the old `bash_lsp_config()` that required bash-language-server to be installed.

### Direct client tests (`tests/lsp_integration.rs`)

Tests exercise `LspClient` directly against mockls, verifying client-side protocol handling without the bridge layer.

## Running mockls Tests

```bash
# All mockls tests
make test T=mockls

# Sync roots tests (now use mockls)
make test T=test_sync_roots

# Full suite (includes all mockls + real-server smoke tests)
make test
```

## Relationship to Real-Server Tests

All existing tests that use real language servers remain in the suite. They serve a different purpose: verifying Catenary works with actual LSP implementations. They continue to skip when the server isn't installed. mockls tests and real-server tests are complementary:

- **mockls tests** verify Catenary's protocol handling against a controlled, deterministic server. They always run.
- **Real-server tests** verify end-to-end behavior against production LSP implementations. They run when servers are available.

## Source

- `src/bin/mockls.rs` — the mock server binary and its unit tests
- `Cargo.toml` — `[[bin]]` entry for mockls

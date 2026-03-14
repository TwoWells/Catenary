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
| `--no-push-diagnostics` | off | Never publish push diagnostics (`textDocument/publishDiagnostics`) |
| `--pull-diagnostics` | off | Advertise `diagnosticProvider` and handle `textDocument/diagnostic` requests |
| `--diagnostics-on-save` | off | Only publish diagnostics on `didSave`, not `didOpen`/`didChange` |
| `--drop-after <n>` | none | Close stdout after n responses (simulate crash) |
| `--hang-on <method>` | none | Never respond to this method (repeatable) |
| `--fail-on <method>` | none | Return `InternalError` (-32603) for this method (repeatable) |
| `--send-configuration-request` | off | Send `workspace/configuration` request after initialize |
| `--publish-version` | off | Include `version` field in `publishDiagnostics` notifications |
| `--progress-on-change` | off | Send `$/progress` tokens around diagnostic computation on `didChange` |
| `--cpu-busy <ms>` | none | Burn CPU for N milliseconds after `didChange` without sending notifications |
| `--flycheck-command <cmd>` | none | Spawn subprocess on `didSave` under a `$/progress` bracket (simulates cargo check) |
| `--flycheck-ticks <n>` | none | Override `--ticks` passed to the flycheck subprocess |
| `--advertise-save` | off | Include `textDocumentSync.save` in capabilities (required for `didSave`) |
| `--notification-log <path>` | none | Write every received notification to a JSONL file for test verification |
| `--content-modified-once` | off | Return `ContentModified` (-32801) on first `textDocument/definition`, then succeed |
| `--cpu-on-workspace-change <ms>` | none | Burn CPU on `workspace/didChangeWorkspaceFolders` |
| `--cpu-on-initialized <ms>` | none | Burn CPU on `initialized` before indexing simulation |
| `--log-init-params <path>` | none | Write the `initialize` request params JSON to a file |
| `--scan-roots` | off | Scan workspace roots on initialize and folder changes, index files by extension |
| `--no-code-actions` | off | Omit `codeActionProvider` capability; return empty code action arrays |
| `--multi-fix` | off | Return two quickfix actions per diagnostic (primary + alternative) |

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
| `textDocument/definition` | Scans for definition pattern (`fn`, `function`, `def`, `let`, `const`, `var`, `struct`, `class`, `enum`, `interface`, `trait`, `mod`, `module`, `type`, `method`, `field`); import-scoped resolution; cross-file fallback |
| `textDocument/typeDefinition` | Resolves type via `: TypeName` annotation, searches for type declaration pattern |
| `textDocument/references` | Returns all positions where the word appears across all documents |
| `textDocument/implementation` | Same as references |
| `textDocument/documentSymbol` | Scans for lines matching keyword patterns, returns `DocumentSymbol` array |
| `textDocument/codeAction` | Returns quickfix actions for diagnostics with source "mockls"; always includes a `refactor` action (to exercise kind filtering). Controlled by `--no-code-actions` and `--multi-fix` |
| `textDocument/prepareCallHierarchy` | Returns call hierarchy item for symbol at position |
| `callHierarchy/incomingCalls` | Searches for call sites by scanning for the symbol name in enclosing functions |
| `callHierarchy/outgoingCalls` | Returns empty array |
| `textDocument/prepareTypeHierarchy` | Returns type hierarchy item for symbol at position |
| `typeHierarchy/subtypes` | Returns all struct/class declarations across documents |
| `typeHierarchy/supertypes` | Returns empty array |
| `workspace/symbol` | Searches across all stored documents |

### Notifications (no response)

| Method | Behavior |
|---|---|
| `initialized` | Starts indexing simulation if `--indexing-delay` is set |
| `textDocument/didOpen` | Stores content, publishes diagnostics (unless suppressed) |
| `textDocument/didChange` | Updates content, republishes diagnostics (unless suppressed) |
| `textDocument/didSave` | Publishes diagnostics (unless `--no-push-diagnostics`) |
| `textDocument/didClose` | Removes document from store |
| `workspace/didChangeWorkspaceFolders` | Accepted silently |
| `exit` | Exits the process |

### Server-to-client messages

| Message | When |
|---|---|
| `textDocument/publishDiagnostics` | One warning per document on line 0: "mockls: mock diagnostic" |
| `window/workDoneProgress/create` | Before indexing simulation |
| `$/progress` (begin/end) | During indexing simulation (`--indexing-delay`) or around diagnostics (`--progress-on-change`) |
| `workspace/configuration` | If `--send-configuration-request` is set |

## Diagnostics Trigger Behavior

mockls never publishes diagnostics spontaneously at startup — only in response to document events. This models the pattern where `has_published_diagnostics` stays false during warmup.

| Config | didOpen | didChange | didSave |
|---|---|---|---|
| Default | publishes | publishes | publishes |
| `--diagnostics-on-save` | no | no | publishes |
| `--no-push-diagnostics` | no | no | no |
| `--diagnostics-delay <ms>` | publishes after delay | publishes after delay | publishes after delay |
| `--publish-version` | version field included | version field included | version field included |
| `--progress-on-change` | no | progress + publishes | no |
| `--cpu-busy <ms>` | no | burns CPU, no publish | no |

These map to specific code paths in Catenary's `wait_for_diagnostics_update`:

- **Default:** Server publishes promptly on `didOpen`, exercises Phase 1 generation advance via the `ProcessMonitor` strategy (no progress tokens, no version).
- **`--diagnostics-on-save`:** Server ignores `didOpen`/`didChange`. Catenary sends `didSave` unconditionally after every change, which triggers mockls to publish.
- **`--no-push-diagnostics`:** Exercises the "never published" grace period timeout path. Catenary handles servers that never emit push diagnostics without hanging.
- **`--diagnostics-delay`:** Diagnostics arrive late, exercises Phase 1 activity tracking.
- **`--publish-version`:** Exercises the `Version` strategy — Catenary waits for `publishDiagnostics` with a version field, matching generation advance.
- **`--progress-on-change`:** Exercises the `TokenMonitor` strategy — Catenary waits for `$/progress` Active -> Idle cycle around diagnostic computation.
- **`--cpu-busy`:** Exercises the `ProcessMonitor` strategy under load — server burns CPU without sending progress or diagnostics, testing trust-based patience decay.

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

The test suite uses no real language servers. Real-server validation happens
organically every session via `catenary monitor` — the operator sees every tool
call, every diagnostics cycle, every progress token. That's the smoke test. It
runs on real projects with real contention, which is better coverage than any
synthetic test.

The test suite's job is to verify the **model** — deterministically, on every
machine, under any load. That means mockls + mockc only. Tests that previously
used real servers (`require_rust_analyzer!()`, `require_bash_lsp!()`, etc.)
were migrated to mockls or deleted. The `require_*` guard pattern — where a
test silently skips if the server isn't installed — means CI coverage depends
on what's installed on the runner. That's not testing.

## mockc: Simulated Compiler Subprocess

Real rust-analyzer delegates expensive work to `cargo check` → `rustc`. The LSP
process goes to Sleeping while the child does the work. This is the exact
scheduling pattern that wall-clock timeouts get wrong: the LSP has flat ticks
(Sleeping), but real work is happening in a subprocess.

mockc (`tools/mockc.rs`) replaces this with a deterministic binary. Its only
job: burn CPU for a precise number of ticks, then exit. All durations are in CPU
ticks (centiseconds, 100 Hz) — the same unit `ProcessMonitor` measures. On an
idle machine, 50 ticks takes ~500ms wall time. On a loaded machine it takes
longer, but the tick count is identical. Catenary's failure detector counts
ticks, mockc produces ticks. The units match end-to-end.

```
mockc [OPTIONS]

Options:
  --ticks <N>         Burn CPU for N ticks / centiseconds (default: 10)
  --exit-code <N>     Exit with this code (default: 0)
  --output <text>     Write text to stdout before exiting
  --hang              Never exit (simulate stuck compiler)
```

### Test profiles using mockc

| Profile | mockls flags | mockc flags | Purpose |
|---------|-------------|-------------|---------|
| Fast flycheck | `--flycheck-command mockc` | default (10 ticks) | Basic flycheck cycle |
| Near threshold | `--flycheck-command mockc --flycheck-ticks 900` | 900 ticks | 90% of 1000-tick threshold — passes because ticks are in subprocess |
| Stuck compiler | `--flycheck-command "mockc --hang"` | never exits | Verifies failure detection on progress cycle |
| Compiler crash | `--flycheck-command "mockc --exit-code 1"` | immediate fail | Progress End with no diagnostics |

## Source

- `tools/mockls.rs` — the mock server binary and its unit tests
- `tools/mockc.rs` — the simulated compiler subprocess
- `Cargo.toml` — `[[bin]]` entries for both

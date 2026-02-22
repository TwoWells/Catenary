# Architecture

## Workspace Roots

Catenary accepts multiple workspace roots via the `-r`/`--root` flag:

```bash
catenary -r ./frontend -r ./backend serve
```

If no roots are specified, the current directory is used. Roots can also be
provided dynamically by the MCP client via the `roots/list` protocol.

### One Server Per Language, All Roots

Catenary spawns **one LSP server per language** and passes all roots as
`workspaceFolders` in the LSP `initialize` request. This mirrors how VS Code
and other multi-root editors work — the LSP specification added
`workspaceFolders` and `workspace/didChangeWorkspaceFolders` specifically for
this use case.

When roots change at runtime (via MCP `roots/list_changed`), Catenary sends a
single `workspace/didChangeWorkspaceFolders` notification to each active server
with the added and removed folders.

### Why Not One Server Per Root?

A natural question is whether each root should get its own LSP server instance
to avoid symbol conflicts. Catenary deliberately does **not** do this:

- **LSP servers handle multi-root internally.** Mature servers like
  rust-analyzer, gopls, and pyright discover independent project configurations
  (`Cargo.toml`, `go.mod`, `tsconfig.json`) within each workspace folder and
  treat them as separate compilation units. A `Config` struct in root A and a
  `Config` struct in root B are tracked as distinct types — find-references,
  go-to-definition, and rename all respect project boundaries.

- **Cross-project navigation would break.** Monorepos and library-plus-consumer
  setups rely on a single server seeing all roots to resolve cross-project
  imports and references.

- **Catenary is a transport bridge, not a language engine.** It does not
  understand language semantics and cannot correctly scope results. Imposing its
  own boundaries would conflict with the server's semantic model.

### Where This Can Break Down

- **Weak multi-root support:** Not all LSP servers handle `workspaceFolders`
  well. Some treat the first root as primary and partially ignore the rest.
  This is a server quality issue, not a Catenary limitation.

- **Agent confusion:** An AI agent receiving search results that span two
  unrelated projects might not realize the results come from different
  codebases. File paths in results carry this information, but the agent must
  interpret them correctly.

If two projects are truly unrelated, running them in separate Catenary sessions
is the cleanest solution.

## Path Security

All file operations pass through a `PathValidator` that enforces workspace root
boundaries. A path must be a descendant of at least one root to be accessed.
Symlinks are resolved (canonicalized) before validation, preventing escapes via
symlink traversal.

Catenary's own configuration files (`.catenary.toml`,
`~/.config/catenary/config.toml`) are additionally protected from write access,
preventing agents from modifying their own tool configuration.

## LSP Multiplexing

Catenary routes MCP tool calls to the correct LSP server based on file
extension. The agent never needs to know which server handles which language —
a `hover` request on a `.rs` file routes to rust-analyzer, while the same
request on a `.py` file routes to pyright.

Servers are started eagerly at launch for languages detected in the workspace.
If a request arrives for a language whose server is not yet running, Catenary
spawns it on demand. Dead servers are automatically restarted on the next
request.

## Diagnostics Consistency

LSP has two interaction models. Request/response operations — hover,
go-to-definition, document symbols — return consistent results directly: the
server computes the answer on demand and sends it back. Diagnostics work
differently. Servers push them asynchronously via `textDocument/publishDiagnostics`
whenever analysis completes, and Catenary caches whatever arrived last.

This creates a consistency problem. After a file change is sent to the server,
there is a window where the diagnostics cache still holds results from before
the change. If the result is returned during this window, the agent receives
stale diagnostics and may proceed unaware of errors it just introduced.

Catenary buffers this eventually consistent gap to ensure diagnostics are
current before returning them. Each URI has a generation counter that
increments every time `publishDiagnostics` arrives for it. Before sending a
change notification (`didOpen`/`didChange`) to the server, Catenary snapshots
the counter. After sending, it waits for the server to publish diagnostics for
that URI — advancing the counter past the snapshot — before reading the cache
and returning results. Because the snapshot is taken before the change is sent,
there is no race window: any publication that arrives after the snapshot
necessarily reflects the change or something newer.

The wait is split into two phases. Phase 1 uses a strategy selected per-server
based on runtime observations:

- **Version** — the server includes a `version` field in `publishDiagnostics`.
  Catenary waits for the generation counter to advance past the snapshot. This
  is the strongest signal but has not been observed from any server in practice.
- **`TokenMonitor`** — the server sends `$/progress` tokens (e.g.,
  rust-analyzer's flycheck). Catenary waits for the server to cycle from Active
  to Idle, indicating analysis is complete. A hard timeout prevents infinite
  hangs if the server never starts work for a given change.
- **`ProcessMonitor`** — the server sends neither version nor progress tokens.
  Catenary polls the server process's CPU time via `/proc/<pid>/stat` (Linux)
  or `ps` (macOS) to infer activity. Trust-based patience decays on consecutive
  timeouts without diagnostics arriving (120s → 60s → 30s → 5s), preventing
  long waits on servers that consistently don't produce diagnostics for certain
  change patterns.

Phase 2 is a 2-second activity settle, shared by all strategies. After Phase 1
signals completion, Catenary continues observing the server's notification
stream and progress state. Only when the server has been completely silent for
2 seconds with no active progress tokens does Catenary read the cache and
return. This catches servers like rust-analyzer that publish diagnostics in
multiple rounds — fast warnings from native analysis followed by slower
type-checking errors from flycheck.

This mechanism applies to the paths that return diagnostics after a change:
the `catenary release` hook (for post-edit diagnostics) and the `diagnostics`
tool. Request/response tools like `hover` and `document_symbols` do not need
it — their results come directly from the server response, not from the cache.

## Root Synchronization

When the MCP client sends a `notifications/roots/list_changed` notification,
Catenary:

1. Sends a `roots/list` request to the client to fetch the current roots.
2. Diffs the new roots against the current set.
3. Updates the `PathValidator` security boundary.
4. Sends a batched `workspace/didChangeWorkspaceFolders` notification to each
   active LSP server.
5. Spawns any newly needed LSP servers for languages detected in the added
   roots.

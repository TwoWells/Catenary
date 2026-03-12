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

LSP has two interaction models. Request/response operations — go-to-definition,
document symbols — return consistent results directly: the server computes the
answer on demand and sends it back. Diagnostics work differently. Servers push
them asynchronously via `textDocument/publishDiagnostics` whenever analysis
completes, and Catenary caches whatever arrived last.

This creates a consistency problem. After a file change is sent to the server,
there is a window where the diagnostics cache still holds results from before
the change. If the result is returned during this window, the agent receives
stale diagnostics and may proceed unaware of errors it just introduced.

Catenary buffers this gap to ensure diagnostics are current before returning
them. Each URI has a generation counter that increments every time
`publishDiagnostics` arrives for it. Before sending a change notification
(`didOpen`/`didChange`) to the server, Catenary snapshots the counter. After
sending, it waits for the server to publish diagnostics for that URI —
advancing the counter past the snapshot — before reading the cache and
returning results.

The wait uses a strategy selected per-server based on runtime observations:

- **Version** — the server includes a `version` field in `publishDiagnostics`.
  Catenary waits for a version match — causal proof the server processed the
  change.
- **`TokenMonitor`** — the server sends `$/progress` tokens (e.g.,
  rust-analyzer's flycheck). Catenary waits for the Active → Idle transition.

Servers that provide neither version nor progress do not participate in the
diagnostics lifecycle. They still receive `didOpen`/`didChange` for code
intelligence but no diagnostics are returned.

Failure detection uses CPU ticks instead of wall-clock timeouts. A
`ProcessMonitor` (backed by `/proc/<pid>/stat` on Linux, `proc_pidinfo` on
macOS, `GetProcessTimes` on Windows) tracks the server's CPU consumption. Only
unexplained work — Running with advancing ticks and no active `$/progress`
tokens — drains the failure threshold (1000 ticks = 10 CPU-seconds). Sleeping,
blocked, starved, and progress-explained ticks are free waits.

`didSave` is gated on `textDocumentSync.save` from `ServerCapabilities`. Only
servers that advertise save support receive `textDocument/didSave`.

This mechanism applies to the paths that return diagnostics after a change:
the `catenary hook post-tool` hook (for post-edit diagnostics) and the `diagnostics`
tool. Request/response tools (`definition`, `document_symbols`) do not need
it — their results come directly from the server response, not from the cache.

The wait model uses CPU-tick failure detection and LSP version matching
to determine when the server has finished processing an edit.

## State Management

All persistent state lives in a single SQLite database at
`~/.local/state/catenary/catenary.db`. WAL mode is enabled for concurrent
read access (the TUI and CLI commands can query while the MCP server writes).

### Schema

| Table | Purpose |
|-------|---------|
| `sessions` | Session metadata (ID, PID, display name, client info, timestamps, alive flag) |
| `workspace_roots` | Workspace root paths per session |
| `events` | Session events (tool calls, diagnostics, server state changes, etc.) |
| `language_servers` | Per-session LSP server state |
| `filter_history` | TUI filter patterns (per-workspace) |
| `root_sync_state` | Transcript offset and discovered roots for `sync-roots` |
| `meta` | Schema version tracking |

### Connection owners

| Process | Lifetime |
|---------|----------|
| MCP server | Process lifetime (shared via `Arc<Mutex<Connection>>`) |
| CLI hooks (`notify`, `sync-roots`) | Single command invocation |
| CLI commands (`list`, `monitor`, `query`, `gc`) | Single command invocation |
| TUI dashboard | Dashboard lifetime |

### CLI commands

`catenary query` provides ad-hoc event querying for debugging and bug reports:

```bash
catenary query --session 029ba740 --since 1h
catenary query --kind diagnostics --since today
catenary query --search "hover" --format json
catenary query --sql "SELECT * FROM events WHERE payload LIKE '%timeout%'"
```

`catenary gc` manages data retention:

```bash
catenary gc --older-than 7d
catenary gc --dead
catenary gc --session 029ba740
```

## Root Synchronization

When the MCP client sends a `notifications/roots/list_changed` notification,
Catenary:

1. Sends a `roots/list` request to the client to fetch the current roots.
2. Diffs the new roots against the current set.
3. Updates the `PathValidator` security boundary.
4. Sends a batched `workspace/didChangeWorkspaceFolders` notification to each
   active LSP server. When folders are added, the server is proactively marked
   as `Indexing` so that subsequent queries block until re-indexing completes.
5. Spawns any newly needed LSP servers for languages detected in the added
   roots.

The readiness wait after step 4 uses the same `wait_ready()` mechanism as
initial startup. For servers that report `$/progress` (e.g., rust-analyzer),
the wait is event-driven — queries unblock when the progress cycle completes.
For servers without progress support, Catenary falls back to activity settle
(the same `ProcessMonitor`-style polling used by the diagnostics system),
transitioning to `Ready` once the server has been quiet for 2 seconds.

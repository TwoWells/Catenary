# Wait Model

Catenary's diagnostics contract: after an agent edits a file, Catenary delivers
**trusted diagnostics** or **nothing**. Stale diagnostics are worse than no
diagnostics — an agent that receives stale errors chases phantom problems, burns
usage limits, and may introduce regressions fixing issues that don't exist.

## Two Interaction Patterns

LSP has two fundamentally different patterns. Catenary handles both, but they
have different wait characteristics.

**Request/Response** (definition, references, symbols): Catenary sends a
request, the server responds when ready. The request **blocks**. Catenary's job
is to wait for the response and detect failure if the server is stuck or dead.

**Server Push** (diagnostics via `publishDiagnostics`): The server decides when
to send diagnostics. Catenary has no request to wait on — it watches for an
unsolicited notification. This requires a dedicated wait model with strategies
to know when the server has finished.

## Strategies

Each LSP server is assigned a diagnostics strategy based on runtime observations.
Strategy discovery happens during the first diagnostics cycle.

### Version (Primary)

The server includes a `version` field in `publishDiagnostics` matching the
document version Catenary sent via `didChange`. This is causal proof — the
server processed *this* version and published diagnostics for it.

```
loop:
    select!:
        diagnostics_notify fired →
            if version match → return diagnostics (trusted)
        poll_interval elapsed →
            sample = monitor.sample()
            if dead → return nothing
            if running + ticks advancing + no active progress →
                threshold -= delta   # unexplained work
            if threshold <= 0 → return nothing (server stuck)
            if wall_clock > safety_cap → return nothing (pathological)
```

Exits: version match (trusted), dead (nothing), threshold exhausted (nothing),
safety cap (nothing).

### `TokenMonitor` (Progress Tokens)

The server sends `$/progress` tokens. Catenary waits for the Active → Idle
transition — the server is telling us it finished.

```
loop:
    select!:
        diagnostics_notify or progress_notify fired →
            if generation advanced and ever_active and now idle →
                return diagnostics (trusted)
        poll_interval elapsed →
            poll token monitor state
            sample = monitor.sample()
            (same failure detection as Version)
```

Exits: Active → Idle (trusted), dead (nothing), threshold exhausted (nothing),
safety cap (nothing).

### No Version, No Progress

The server provides neither signal. It still receives `didOpen` and `didChange`
for code intelligence (definition, references, symbols). It does **not** receive
`didSave`. It does not participate in the diagnostics lifecycle. No diagnostics
are returned — not stale, not cached, nothing.

A survey of 280+ language servers (`docs/src/lsp/landscape.md`) confirmed that
every major server includes `version` in `publishDiagnostics` and/or sends
`$/progress` tokens. Both features have been in the LSP spec since 3.15 (2019)
and are provided automatically by every dominant LSP framework
(`vscode-languageserver`, `pygls`, `tower-lsp`, `eclipse.lsp4j`).

## Failure Detection

The failure detector is not a budget. Catenary does not ration CPU time. The
server can use as much CPU as it needs. The failure detector catches a single
failure mode: **the server is consuming CPU silently** — Running with advancing
ticks, but producing no responses, no diagnostics, no progress tokens.

### Units

CPU ticks are normalized to centiseconds (100 Hz) on all platforms:

- **Linux:** `/proc/<pid>/stat` reports `utime + stime` in `USER_HZ` (always
  100). 1 tick = 10ms of CPU time. Stable kernel ABI.
- **macOS:** `proc_pidinfo` with `PROC_PIDTASKINFO` reports Mach absolute time.
  Normalized via `mach_timebase_info()` to centiseconds.
- **Windows:** `GetProcessTimes` reports in 100-nanosecond intervals. Divided
  by 100,000 for centiseconds.

### Thresholds

| Operation | Typical CPU time | Threshold | Headroom |
|-----------|-----------------|-----------|----------|
| Definition / References | 10–150ms | 1000 ticks (10 CPU-sec) | 70–1000x |
| Diagnostics (incremental) | < 1s | 1000 ticks | 10x |
| Flycheck (cargo check) | seconds | Progress-tracked | N/A |
| Full indexing | tens of seconds | Progress-tracked | N/A |

### Progress-Aware Detection

The failure detector distinguishes **explained** from **unexplained** work:

| Server state | Progress | Threshold drains? | Reasoning |
|-------------|----------|-------------------|-----------|
| Running + ticks advancing | Active | **No** | Server is working and telling us |
| Running + ticks advancing | None/Idle | **Yes** | Unexplained — server is silent |
| Running + ticks flat | Any | No | CPU-starved — free wait |
| Sleeping | Any | No | Waiting on I/O or subprocess |
| Blocked (D-state) | Any | No | Kernel I/O — free wait |
| Dead | Any | — | Bail immediately |

Only **Running + ticks advancing + no active progress** drains the threshold.
Everything else is either explained (progress Active), or the server hasn't had
a chance to work (starved, sleeping, blocked).

### Safety Cap

A 5-minute wall-clock circuit breaker for pathological cases only: dead NFS
mounts, processes stuck in D-state, zombies that never get reaped. This should
never fire under normal operation.

## `catenary-proc` Crate

`crates/catenary-proc/` provides the `ProcessMonitor` struct — a stateful
process monitor with persistent OS handles. Created once at server spawn, lives
on `LspClient` for the server's lifetime. Encapsulates tick delta tracking and
amortizes handle open costs.

```rust
impl ProcessMonitor {
    /// Opens persistent handles for the server's lifetime.
    pub fn new(pid: u32) -> Option<Self>;

    /// Returns (delta, state) where delta is ticks since last sample.
    pub fn sample(&mut self) -> Option<(u64, ProcessState)>;
}
```

Platform implementations:

- **Linux:** Persistent `File` handle to `/proc/<pid>/stat`. Seek + read into
  a reused buffer. 2 syscalls per sample.
- **macOS:** `proc_pidinfo` — stateless syscall, no persistent handle needed.
- **Windows:** Persistent `HANDLE` from `OpenProcess`. `CloseHandle` in `Drop`.

## `load_aware_grace` — Unified Wait Infrastructure

`src/lsp/wait.rs` contains the single wait pattern used by all sites —
diagnostics preamble, readiness, request timeouts.

```rust
pub async fn load_aware_grace<S, F, Fut>(
    sample_fn: &mut S,
    threshold: u64,
    max_wall: Option<Duration>,
    notify: &Notify,
    progress_active: impl Fn() -> bool,
    condition: F,
) -> bool
```

Key properties:

- **Event-driven wake:** Condition is checked immediately when the notify fires.
  If diagnostics arrive 1ms into a 200ms poll interval, we wake at 1ms.
- **Progress-aware:** Ticks during active progress are explained work and do not
  drain the threshold.
- **Failure detection, not budgeting:** The threshold is generous relative to
  normal operation. It only fires for genuinely stuck servers.

## `did_save` Gating

Catenary reads `textDocumentSync.save` from `ServerCapabilities`. If the server
advertises save support, Catenary sends `textDocument/didSave` unconditionally
after every change — this is required to trigger diagnostics on servers that
only run analysis on save (e.g., rust-analyzer's flycheck). If the server does
not advertise save, `didSave` is not sent.

## Readiness

`is_ready()` checks that the server is in `ServerState::Ready` and confirms
the process is idle via `ProcessMonitor`. During warmup (first 3 seconds from
spawn), the process must be Sleeping — this catches the window between
initialize completion and the start of indexing. After warmup, Ready state
alone is sufficient.

`wait_ready()` uses `load_aware_grace` with a 1000-tick threshold. For servers
set to Busy proactively (e.g., after workspace folder changes), it detects
activity settle: consecutive samples of Sleeping + flat ticks transition the
server back to Ready.

## Agent-Facing Contract

Catenary's hook fires after an agent edits a file. The agent did not ask for
diagnostics — Catenary injects them into the context. Therefore:

1. **Trusted diagnostics** → inject them.
2. **No diagnostics** → inject nothing. The agent doesn't know the hook ran.
3. **Server problems are never surfaced to the agent.** No "server died", no
   "diagnostics unavailable". Server health is logged for the operator via
   `catenary monitor`.

## Capabilities Policy

Catenary advertises only capabilities it uses. Unnecessary capabilities cause
servers to compute features nobody requested.

**Advertised** (must be present for correct behavior):
`synchronization`, `definition`, `typeDefinition`, `implementation`,
`declaration`, `references`, `documentSymbol`, `callHierarchy`,
`typeHierarchy`, `publishDiagnostics` (with `versionSupport: true`),
`window.workDoneProgress`.

**Not advertised** (tools removed or not applicable to agents):
`hover`, `codeAction`, `rename`, `completion`, `signatureHelp`,
`documentHighlight`, `codeLens`, `documentLink`, `colorProvider`,
`formatting`, `foldingRange`, `selectionRange`, `semanticTokens`,
`inlineValue`, `inlayHint`.

## Design Decisions

### Why CPU ticks, not wall-clock

Wall time is the wrong unit for waiting on a process. On an idle machine, 30s
means 30s of CPU work. Under load (test suite saturating cores), the server
gets scheduled intermittently. A wall-clock timeout fires whether the server got
zero CPU or saturated a core. CPU ticks measure actual work.

### Why failure detection, not budgeting

The threshold does not ration CPU time. It detects a specific pathology: the
server burning CPU silently. A server indexing for 60 CPU-seconds with active
progress never touches the threshold.

### Why progress-aware

If progress is Active, the ticks are explained — the server is working and
telling us. Counting those ticks would create false positives during legitimate
long operations (flycheck, indexing).

### Why Ready + Sleeping = ready

After initialize, the server may be about to start indexing but hasn't sent
`$/progress` yet. With `ProcessMonitor`, we observe the process state directly.
Sleeping means the server has finished initialization and is waiting for
requests. Running means it's still working. This replaces a wall-clock guess
with an observation.

### Why requests block

LSP requests go to a child process on the same machine (no network). If the
server hasn't responded, it's busy, sleeping on a subprocess, dead, or stuck.
The failure detector handles all cases. There is nothing "best-effort" about it.

### Why servers manage their own memory

Language servers use LRU eviction, lazy loading, and internal resource policies.
Catenary does not send unsolicited `didClose` for memory management.

### Why no settle phase

The two-phase model (strategy wait + activity settle) was replaced by
event-driven detection. The Version strategy exits on version match — causal
proof. The `TokenMonitor` strategy exits on Active → Idle. Both are
authoritative signals that don't need a settle confirmation.

### Why no trust decay

Trust decay (120s → 60s → 30s → 5s patience) was a heuristic for the
ProcessMonitor strategy (no version, no progress). That strategy was replaced:
servers without version or progress simply don't participate in diagnostics.

### Why no process tree walking

Subprocess ticks (e.g., cargo check spawned by rust-analyzer) don't count
against the LSP process. The LSP goes to Sleeping while the subprocess runs.
The failure detector sees Sleeping + flat ticks on the LSP — free wait. No
need to walk the process tree.

### Why no sentinel file

A sentinel file approach (write a marker, wait for the server to see it) was
rejected because it requires filesystem coordination, doesn't work for all
servers, and LSP already provides the signals needed (version, progress).

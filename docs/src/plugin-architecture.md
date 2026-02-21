# Plugin Architecture

Catenary ships plugins for two AI CLI hosts from a single repository. Each host
has its own plugin format and file layout, but they share the same `catenary`
binary and MCP server.

## Repository Layout

```
Catenary/
├── .claude-plugin/
│   └── marketplace.json        # Claude Code marketplace metadata
├── plugins/
│   └── catenary/               # Claude Code plugin root
│       ├── .mcp.json           # MCP server declaration
│       ├── hooks/
│       │   └── hooks.json      # Claude Code hooks
│       ├── config.example.toml
│       └── README.md
├── gemini-extension.json       # Gemini CLI extension manifest
├── hooks/
│   └── hooks.json              # Gemini CLI hooks
└── ...
```

The two plugin roots are:

| Host       | Plugin root          | Hooks file                       |
| ---------- | -------------------- | -------------------------------- |
| Claude Code | `plugins/catenary/` | `plugins/catenary/hooks/hooks.json` |
| Gemini CLI | repo root (`/`)      | `hooks/hooks.json`               |

Both hosts expect hooks in a `hooks/hooks.json` file relative to the plugin
root. The manifest file (where the MCP server is declared) is separate from the
hooks file in both cases.

## Claude Code Plugin

Installed via the marketplace:

```bash
claude plugin marketplace add MarkWells-Dev/Catenary
claude plugin install catenary@catenary
```

### Updating after a new release

Claude Code caches plugin files (including `hooks.json`) under
`~/.claude/plugins/cache/` at install time. Updating the `catenary` binary alone
does not refresh the cached hooks. To fully apply a Catenary update, remove and
reinstall the plugin:

```bash
claude plugin remove catenary@catenary
claude plugin install catenary@catenary
```

Then start a new Claude Code session. Running sessions use the hooks that were
cached when the session started, and their MCP server process runs for the
session lifetime — protocol changes require a fresh session.

### Plugin source

`.claude-plugin/marketplace.json` points to the plugin source directory:

```json
"source": "./plugins/catenary"
```

Inside `plugins/catenary/`:

- **`.mcp.json`** — declares the MCP server (`catenary` command).
- **`hooks/hooks.json`** — registers hooks for diagnostics, root sync, and
  file locking:
  - `PreToolUse` (all tools): runs `catenary sync-roots` to pick up `/add-dir`
    workspace additions and directory removals.
  - `PreToolUse` on `Edit|Write|NotebookEdit`: runs `catenary lock acquire` to
    serialize concurrent edits across agents.
  - `PostToolUse` on `Edit|Write|NotebookEdit`: runs `catenary notify` for
    post-edit LSP diagnostics, then `catenary lock track-read` to update the
    tracked mtime (preventing false stale-read warnings on self-edits), then
    `catenary lock release` to start the grace period.
  - `PostToolUse` on `Read`: runs `catenary lock track-read` for change
    detection.
  - `PostToolUseFailure` on `Edit|Write|NotebookEdit`: runs
    `catenary lock release --grace 0` for immediate lock release on failure.
- **`config.example.toml`** — example Catenary configuration.

## Gemini CLI Extension

Installed via:

```bash
gemini extensions install https://github.com/MarkWells-Dev/Catenary
```

The extension root is the repository root. Two files matter:

- **`gemini-extension.json`** — manifest declaring the MCP server. Does **not**
  contain hooks (Gemini CLI ignores hooks defined in the manifest).
- **`hooks/hooks.json`** — registers hooks for diagnostics and file locking:
  - `BeforeTool` on `write_file|replace`: runs
    `catenary lock acquire --format=gemini` to serialize concurrent edits.
  - `AfterTool` on `read_file|write_file|replace`: runs
    `catenary notify --format=gemini` for post-edit LSP diagnostics.
  - `AfterTool` on `write_file|replace`: runs
    `catenary lock track-read --format=gemini` to update the tracked mtime.
  - `AfterTool` on `write_file|replace`: runs
    `catenary lock release --format=gemini` for lock grace period.
  - `AfterTool` on `read_file`: runs
    `catenary lock track-read --format=gemini` for change detection.

## Hook Contracts

All hook commands (`catenary notify`, `catenary sync-roots`, `catenary lock`)
read hook JSON from stdin. They silently succeed on any error to avoid breaking
the host CLI's flow.

### `catenary notify`

Triggered after file edits. Reads the hook JSON, extracts the file path, finds
the matching Catenary session, and returns LSP diagnostics to stdout.

**Fields consumed from hook JSON:**

| Field | Used for |
| ----- | -------- |
| `tool_input.file_path` or `tool_input.file` | File that was edited |
| `cwd` | Resolving relative file paths (fallback: process CWD) |

**Output format** depends on the `--format` flag. Both formats wrap
diagnostics in a `hookSpecificOutput` JSON envelope:

- Default (Claude Code): includes `hookEventName: "PostToolUse"` and
  `additionalContext` for Claude Code's `PostToolUse` hook contract.
- `--format=gemini`: uses `additionalContext` with an "LSP Diagnostics"
  prefix for Gemini CLI's `AfterTool` hooks.

### `catenary sync-roots`

Triggered before each tool use (Claude Code only). Scans the Claude Code
transcript for `/add-dir` additions and directory removals, then sends the full
workspace root set to the running Catenary session. The server diffs against its
current state, applying both additions and removals to LSP clients and the search
index.

State is persisted in `known_roots.json` (inside the session directory) to track
the transcript byte offset and the full discovered root set across invocations.

**Fields consumed from hook JSON:**

| Field | Used for |
| ----- | -------- |
| `transcript_path` | Path to the Claude Code transcript file |
| `cwd` | Identifying which Catenary session to update |

### `catenary lock acquire`

Triggered before file edits (Claude Code `PreToolUse`, Gemini `BeforeTool`). Acquires a file-level
advisory lock, blocking until the lock is available or the timeout expires. This
serializes concurrent edits to the same file across multiple agents.

**Fields consumed from hook JSON:**

| Field | Used for |
| ----- | -------- |
| `session_id` | Lock owner identity (primary key) |
| `agent_id` | Lock owner identity (appended if present) |
| `tool_input.file_path` or `tool_input.file` | File to lock |
| `cwd` | Resolving relative file paths and finding the session for monitor events |

**Flags:**

| Flag | Required | Description |
| ---- | -------- | ----------- |
| `--timeout` | no (default 180) | Seconds to wait before giving up |
| `--format` | yes | Output format (`claude` or `gemini`) |

**Output:** silent on success. On timeout, returns JSON with
`permissionDecision: "deny"`. If the file was modified since the owner's last
read, returns JSON with `additionalContext` warning.

### `catenary lock release`

Triggered after file edits (Claude Code `PostToolUse`, Gemini `AfterTool`).
Releases the lock with a grace period, allowing the same agent to re-acquire
without contention during the diagnostics→fix cycle.

**Fields consumed from hook JSON:**

| Field | Used for |
| ----- | -------- |
| `session_id` | Lock owner identity |
| `agent_id` | Lock owner identity (appended if present) |
| `tool_input.file_path` or `tool_input.file` | File to unlock |
| `cwd` | Finding the session for monitor events |

**Flags:**

| Flag | Required | Description |
| ---- | -------- | ----------- |
| `--grace` | no (default 30) | Seconds before the lock expires |

### `catenary lock track-read`

Triggered after file reads (Claude Code `PostToolUse` on `Read`, Gemini
`AfterTool` on `read_file`). Records the file's modification time so future
lock acquisitions can detect if the file changed since the agent last read it.

**Fields consumed from hook JSON:**

| Field | Used for |
| ----- | -------- |
| `session_id` | Owner identity for tracking |
| `agent_id` | Owner identity (appended if present) |
| `tool_input.file_path` or `tool_input.file` | File to track |

## Version Management

Three files carry the version number:

| File | Field |
| ---- | ----- |
| `Cargo.toml` | `version` |
| `.claude-plugin/marketplace.json` | `plugins[0].version` |
| `gemini-extension.json` | `version` |

The `make release-*` targets bump all three atomically. A `version_sync` test
(`tests/version_sync.rs`) verifies they stay in sync.

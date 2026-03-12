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
- **`hooks/hooks.json`** — registers hooks for diagnostics and root sync:
  - `PreToolUse` (all tools): runs `catenary hook pre-tool` to pick up `/add-dir`
    workspace additions and directory removals.
  - `PostToolUse` on `Edit|Write|NotebookEdit|Read`: runs `catenary hook post-tool`
    to return LSP diagnostics after file operations.
- **`config.example.toml`** — example Catenary configuration.

## Gemini CLI Extension

Installed via:

```bash
gemini extensions install https://github.com/MarkWells-Dev/Catenary
```

The extension root is the repository root. Two files matter:

- **`gemini-extension.json`** — manifest declaring the MCP server. Does **not**
  contain hooks (Gemini CLI ignores hooks defined in the manifest).
- **`hooks/hooks.json`** — registers hooks for diagnostics:
  - `AfterTool` on `read_file|write_file|replace`: runs
    `catenary hook post-tool --format=gemini` to return LSP diagnostics after
    file operations.

## Hook Contracts

All hook commands (`catenary hook post-tool`, `catenary hook pre-tool`) read
hook JSON from stdin. They silently succeed on any error to avoid breaking the
host CLI's flow.

### `catenary hook post-tool`

Triggered after file reads or edits (Claude Code `PostToolUse`, Gemini
`AfterTool`). Connects to the session's hook socket and returns LSP diagnostics
to stdout.

**Fields consumed from hook JSON:**

| Field | Used for |
| ----- | -------- |
| `tool_input.file_path` or `tool_input.file` | File to check for diagnostics |
| `cwd` | Resolving relative file paths and finding the session |

**Flags:**

| Flag | Required | Description |
| ---- | -------- | ----------- |
| `--format` | yes | Output format (`claude` or `gemini`) |

**Output:** silent when no diagnostics. Otherwise returns JSON with
`additionalContext` containing the diagnostic text.

### `catenary hook pre-tool`

Triggered before each tool use (Claude Code only). Scans the Claude Code
transcript for `/add-dir` additions and directory removals, then sends the full
workspace root set to the running Catenary session. The server diffs against its
current state, applying both additions and removals to LSP clients and the search
index.

State is persisted in the `root_sync_state` table in the SQLite database to track
the transcript byte offset and the full discovered root set across invocations.

**Fields consumed from hook JSON:**

| Field | Used for |
| ----- | -------- |
| `transcript_path` | Path to the Claude Code transcript file |
| `cwd` | Identifying which Catenary session to update |

## Version Management

Three files carry the version number:

| File | Field |
| ---- | ----- |
| `Cargo.toml` | `version` |
| `.claude-plugin/marketplace.json` | `plugins[0].version` |
| `gemini-extension.json` | `version` |

The `make release-*` targets bump all three atomically. A `version_sync` test
(`tests/version_sync.rs`) verifies they stay in sync.

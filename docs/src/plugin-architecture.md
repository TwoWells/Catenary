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

`.claude-plugin/marketplace.json` points to the plugin source directory:

```json
"source": "./plugins/catenary"
```

Inside `plugins/catenary/`:

- **`.mcp.json`** — declares the MCP server (`catenary` command).
- **`hooks/hooks.json`** — registers two hooks:
  - `PostToolUse` on `Edit|Write|NotebookEdit`: runs `catenary notify` for
    post-edit LSP diagnostics.
  - `PreToolUse` (all tools): runs `catenary sync-roots` to pick up `/add-dir`
    workspace additions.
- **`config.example.toml`** — example Catenary configuration.

## Gemini CLI Extension

Installed via:

```bash
gemini ext install markwells.catenary
```

The extension root is the repository root. Two files matter:

- **`gemini-extension.json`** — manifest declaring the MCP server. Does **not**
  contain hooks (Gemini CLI ignores hooks defined in the manifest).
- **`hooks/hooks.json`** — registers one hook:
  - `AfterTool` on `read_file|write_file|replace`: runs
    `catenary notify --format=gemini` for post-edit LSP diagnostics.

## Hook Contracts

Both `catenary notify` and `catenary sync-roots` read hook JSON from stdin.
They silently succeed on any error to avoid breaking the host CLI's flow.

### `catenary notify`

Triggered after file edits. Reads the hook JSON, extracts the file path, finds
the matching Catenary session, and returns LSP diagnostics to stdout.

**Fields consumed from hook JSON:**

| Field | Used for |
| ----- | -------- |
| `tool_input.file_path` or `tool_input.file` | File that was edited |
| `cwd` | Resolving relative file paths (fallback: process CWD) |

**Output format** depends on the `--format` flag:

- Default (Claude Code): plain text, one diagnostic per line.
- `--format=gemini`: wrapped in `<hook_context>` tags for Gemini's context
  injection.

### `catenary sync-roots`

Triggered before each tool use (Claude Code only). Scans the Claude Code
transcript for `/add-dir` confirmations and sends newly discovered roots to the
running Catenary session.

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

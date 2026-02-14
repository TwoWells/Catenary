# CLI Integration

Integrate catenary-mcp with existing AI coding assistants (Claude Code, Gemini
CLI) by disabling their built-in tools and replacing them with catenary's
LSP-backed alternatives.

## Why Not a Custom CLI?

The original plan was to build `catenary-cli` to control the model agent loop.
This was abandoned because:

**Subscription plans are tied to official CLI tools.** Claude Code and Gemini
CLI use subscription billing ($20/month Pro tier). A custom CLI would require
API keys with pay-per-token billing — different billing system, higher cost for
the target audience (individual developers).

**The constraint we wanted is achievable without a custom CLI.** Both tools
support:

1. Disabling built-in tools
2. Adding MCP servers as replacements
3. Workspace-level configuration

We get the same outcome — model forced to use catenary tools — without
maintaining a CLI.

## Design Principles

Preserved from the original CLI design:

### No Arbitrary Shell

No `shell` tool. Every action goes through a targeted MCP tool.

**Why:**

- Model can't bypass `search` with raw `grep`
- Model can't `cat` files instead of using `read_file`
- No accidental `rm -rf` or destructive commands
- Every action is intentional and auditable
- Token efficient — no parsing noisy shell output

### LSP-First

- Hover instead of file read (for type info)
- Symbols instead of grep (for definitions)
- Diagnostics on write (catch errors immediately)

### Efficient

- Every token counts — users are on Pro tier, not unlimited
- LSP queries cost fewer tokens than file reads
- Diagnostics prevent wasted cycles on broken code

## Configuration

### Gemini CLI

Location: `.gemini/settings.json` (workspace) or `~/.gemini/settings.json`
(user)

**Key finding:** Use `tools.core` (allowlist). `tools.exclude` doesn't work
reliably.

```json
{
  "tools": {
    "core": ["web_fetch", "google_web_search", "save_memory"]
  },
  "mcpServers": {
    "catenary": {
      "command": "catenary"
    }
  }
}
```

**Built-in tool names** (from `packages/core/src/tools/tool-names.ts`):

| Tool              | Internal Name       |
| ----------------- | ------------------- |
| LSTool            | `list_directory`    |
| ReadFileTool      | `read_file`         |
| WriteFileTool     | `write_file`        |
| EditTool          | `replace`           |
| GrepTool          | `grep_search`       |
| GlobTool          | `glob`              |
| ReadManyFilesTool | `read_many_files`   |
| ShellTool         | `run_shell_command` |
| WebFetchTool      | `web_fetch`         |
| WebSearchTool     | `google_web_search` |
| MemoryTool        | `save_memory`       |

### Claude Code

Location: `.claude/settings.json` (workspace) or `~/.claude/settings.json`
(user)

**Key finding:** Must deny `Task` to prevent sub-agent escape hatch.

```json
{
  "permissions": {
    "deny": [
      "Read",
      "Edit",
      "Write",
      "Bash",
      "Grep",
      "Glob",
      "Task",
      "NotebookEdit"
    ],
    "allow": [
      "WebSearch",
      "WebFetch",
      "mcp__catenary__*",
      "ToolSearch",
      "AskUserQuestion"
    ]
  },
  "mcpServers": {
    "catenary": {
      "command": "catenary"
    }
  }
}
```

## Experiment Results

Validated 2026-02-06.

| Test                     | Gemini CLI             | Claude Code                        |
| ------------------------ | ---------------------- | ---------------------------------- |
| Restriction method       | `tools.core` allowlist | `permissions.deny` + block `Task`  |
| MCP tools discovered     | ✓                      | ✓                                  |
| Built-in tools blocked   | ✓                      | ✓                                  |
| Model adapts gracefully  | ✓ (slowly)             | ✓ (quickly)                        |
| Sub-agent escape blocked | N/A                    | ✓ (requires denying `Task`)        |

### Model Behavior When Constrained

**Gemini:**

1. Tried WebFetch to read local file (wrong tool)
2. Tried `run_shell_command` → blocked
3. Adapted to `codebase_map`
4. Used `document_symbols`
5. Delegated to sub-agent (burned tokens before admitting defeat)

**Claude:**

1. Tried `Skill(read)` → failed
2. No Read/Bash/Task available
3. Adapted to `document_symbols`
4. Admitted limitations gracefully
5. Offered LSP-based alternatives

**Key difference:** Claude admits defeat faster and communicates limitations
clearly. Gemini burns tokens trying workarounds.

## Required Catenary Tools

For full functionality, catenary-mcp needs file I/O tools:

| Tool                      | Status    | Notes                    |
| ------------------------- | --------- | ------------------------ |
| `catenary_read_file`      | ❌ TODO   | Essential for any task   |
| `catenary_write_file`     | ❌ TODO   | With diagnostics         |
| `catenary_edit_file`      | ❌ TODO   | With diagnostics         |
| `catenary_list_directory` | ❌ TODO   | Basic navigation         |
| `find_symbol`    | ✓ Exists  | LSP workspace symbols    |
| `find_references`| ✓ Exists  | LSP references           |
| `codebase_map`   | ✓ Exists  | File tree with symbols   |
| `document_symbols`    | ✓ Exists  | File structure           |
| `hover`               | ✓ Exists  | Type info, docs          |
| `diagnostics`         | ✓ Exists  | Errors, warnings         |

## Limitations

### No Shell Fallback

By design. Models can't escape to grep/cat/shell. This is the feature.

When catenary lacks a tool the model needs, it must either:

- Use available catenary tools creatively
- Admit it can't complete the task

This surfaces gaps in catenary's tool coverage rather than hiding them behind
shell escapes.

### LSP Dependency

Some operations require LSP:

- Find references (no grep fallback currently)
- Rename symbol
- Code actions / quick fixes

If LSP is unavailable for a language, these tools return errors. Future work
may add grep fallbacks with degradation notices.

## See Also

- [Archive: CLI Design](archive/cli-design.md) — Original custom CLI design
  (abandoned)
- [Configuration](configuration.md) — catenary-mcp configuration reference

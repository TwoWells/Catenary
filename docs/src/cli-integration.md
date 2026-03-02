# CLI Integration

Integrate catenary-mcp with existing AI coding assistants (Claude Code, Gemini
CLI) by constraining their built-in tools so the model uses catenary's
LSP-backed navigation instead of text scanning.

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

Location: `~/.gemini/settings.json` (user) or `.gemini/settings.json` (workspace)

**Recommended: Extension + Constrained Mode.**

1.  **Install the Extension:** The Catenary extension provides
    `BeforeTool` / `AfterTool` hooks that run `catenary acquire` /
    `catenary release` around file operations. This ensures file locking
    and the model sees LSP diagnostics immediately.

    ```bash
    gemini extensions install https://github.com/MarkWells-Dev/Catenary
    ```

2.  **Constrained mode.** Use the `constrained_bash.py` hook to deny
    text-scanning commands while keeping Gemini's native file I/O and
    shell tools available.

**Install:**

```bash
cp /path/to/catenary/scripts/constrained_bash.py ~/.gemini/hooks/constrained_bash.py
chmod +x ~/.gemini/hooks/constrained_bash.py
```

**Configure** in `~/.gemini/settings.json`:

```json
{
  "hooks": {
    "BeforeTool": [
      {
        "matcher": "run_shell_command",
        "hooks": [
          {
            "type": "command",
            "command": "$HOME/.gemini/hooks/constrained_bash.py --format=gemini"
          }
        ]
      }
    ]
  },
  "mcpServers": {
    "catenary": {
      "command": "catenary"
    }
  }
}
```

The `--format=gemini` flag switches the denial response to Gemini's
`{"decision": "deny", "reason": "..."}` format.

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

**Recommended: Hook-based integration.** Claude Code's native `Read`, `Edit`,
and `Write` tools handle file I/O with inline diffs and syntax highlighting.
Catenary provides file locking and LSP diagnostics via `PreToolUse` /
`PostToolUse` hooks — the lock is held through the full edit→diagnostics cycle.

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Edit|Write|NotebookEdit|Read",
        "hooks": [
          {
            "type": "command",
            "command": "catenary acquire --format=claude"
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "Edit|Write|NotebookEdit|Read",
        "hooks": [
          {
            "type": "command",
            "command": "catenary release --format=claude"
          }
        ]
      }
    ],
    "PostToolUseFailure": [
      {
        "matcher": "Edit|Write|NotebookEdit|Read",
        "hooks": [
          {
            "type": "command",
            "command": "catenary release --grace 0"
          }
        ]
      }
    ]
  },
  "mcpServers": {
    "catenary": {
      "command": "catenary"
    }
  }
}
```

The `catenary release` command reads the hook's JSON from stdin, finds the
running Catenary session for the workspace, returns any LSP diagnostics,
records the file's mtime, and releases the lock. It exits silently on any
error so it never blocks Claude Code's flow.

**Alternative: Constrained mode.** Keep Claude Code's native `Read`, `Edit`,
`Write`, and `Bash` tools but block text-scanning commands to force LSP-first
navigation. The repo includes a hook script that implements an allowlist:
only `make`, `git`, `gh`, and a handful of filesystem utilities are permitted.
Everything else is denied with guidance pointing to the appropriate Catenary
tool.

**Install:**

```bash
# Symlink so the live hook stays in sync with the repo
mkdir -p ~/.claude/hooks
ln -s /path/to/catenary/scripts/constrained_bash.py ~/.claude/hooks/constrained_bash.py
chmod +x /path/to/catenary/scripts/constrained_bash.py
```

**Configure** in `~/.claude/settings.json`:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "$HOME/.claude/hooks/constrained_bash.py --format=claude"
          }
        ]
      }
    ]
  }
}
```

The script provides specific guidance per command (`Use Catenary's grep tool
instead.`, `Use the Read tool instead.`, etc.) so the model corrects course
immediately rather than attempting workarounds.

**Troubleshooting:**

- **Hook silently allows blocked commands.** The script must be executable.
  `chmod +x` on the script file (or the repo file if you use a symlink) is
  required. Without it, Claude Code gets a permission error, treats the hook as
  a non-blocking failure, and lets the command through.

- **Hook allows blocked commands and logs a JSON parse error.** Claude Code
  spawns a shell that sources your profile. If `~/.zshrc` or `~/.bashrc` prints
  anything unconditionally (e.g. `fastfetch`, `neofetch`, a startup banner),
  that output is prepended to the hook's JSON and corrupts the parse. Add this
  guard at the very top of both files:

  ```zsh
  # Skip the rest of this file for non-interactive shells (e.g. hook runners)
  [[ -o interactive ]] || return
  ```

- **Blocked commands appear as "Read" in the UI.** Claude Code relabels Bash
  commands that contain `tail`, `head`, or `cat` as a native Read operation in
  the transcript view. The hook still fires on the Bash event and blocks
  correctly — the relabeling is cosmetic only.

This keeps `Bash` available for build/test/git commands while blocking every
path that would let the model fall back to text scanning. The model uses:

- **Catenary `grep`** for content discovery (symbols, references, text matches)
- **Catenary `glob`** for directory browsing (replaces `ls`, `tree`, `find`)
- **Claude Code `Read`/`Edit`/`Write`** for file I/O (with `catenary release` hook for diagnostics)
- **Claude Code `Bash`** for build, test, and git commands only

## Experiment Results

### Current: `constrained_bash.py` hook (both hosts)

Validated 2026-02-17.

| Test                     | Gemini CLI                | Claude Code                                 |
| ------------------------ | ------------------------- | ------------------------------------------- |
| Restriction method       | `BeforeTool` hook         | `PreToolUse` hook                           |
| MCP tools discovered     | ✓                         | ✓                                           |
| Text scanning blocked    | ✓                         | ✓                                           |
| Model adapts gracefully  | ✓ (immediately)           | ✓ (immediately)                             |
| Sub-agent escape blocked | N/A                       | ✓ (requires denying `Task`)                 |

Both hosts use the same `constrained_bash.py` script (with `--format=claude`
or `--format=gemini`). The hook approach was chosen over the hosts' native
permission systems (Gemini's Policy Engine, Claude Code's `permissions.deny`
list) because the Python script can make context-sensitive decisions — for
example, allowing pipeline-safe commands like `head`, `tail`, `sed`, and `awk`
mid-pipeline (reading from stdin) while blocking them at the start of a command
(reading from files). Static deny lists cannot distinguish these cases.

Tested with `gemini-3-flash-preview` and `claude-opus-4-6`. Both adapted
on the first prompt with zero fallback attempts.

### Historical: Policy Engine (Gemini) + Deny List (Claude)

Validated 2026-02-17, superseded by the hook approach above.

Used Gemini's Policy Engine (`deny` rules in TOML) and Claude Code's
`permissions.deny` list. Both gave clear feedback but lacked the flexibility
to allow context-dependent exceptions (e.g. pipeline-safe commands).

### Historical: `tools.core` Allowlist (Gemini, deprecated)

Validated 2026-02-06.

The original Gemini approach used `tools.core` to allowlist only non-file
tools (`web_fetch`, `google_web_search`, `save_memory`), hiding all built-in
file and shell tools. This worked but models adapted slowly — Gemini would
try several workarounds (WebFetch for local files, sub-agent delegation)
before settling on Catenary tools. The policy engine approach replaced this
by giving explicit deny messages instead of silently removing tools.

## Catenary Tool Coverage

Catenary exposes two MCP tools plus post-edit diagnostics via hooks:

| Tool   | Category  | Notes                                              |
| ------ | --------- | -------------------------------------------------- |
| `grep` | LSP+text  | Symbols, references, hover, implementations, text heatmap |
| `glob` | File I/O  | Files, dirs, symlinks, symbol outlines             |
| hooks  | LSP       | Post-edit diagnostics and code actions              |

File I/O is handled by the host tool's native file operations. Catenary
provides post-edit diagnostics via the `catenary release` hook.

## Limitations

### LSP Dependency

Semantic enrichment in `grep` (references, implementations, type cross-refs)
requires a running LSP server. When no LSP server covers a file, `grep` falls
back to text-only matching. Code actions are delivered via hooks and also
require LSP.

## See Also

- [Archive: CLI Design](archive/cli-design.md) — Original custom CLI design
  (abandoned)
- [Configuration](configuration.md) — catenary-mcp configuration reference

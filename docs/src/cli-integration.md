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

Location: `~/.gemini/policies/` (user) or `.gemini/settings.json` (workspace)

**Recommended: Extension + Constrained Mode.**

1.  **Install the Extension:** The Catenary extension provides
    `BeforeTool` / `AfterTool` hooks that run `catenary acquire` /
    `catenary release` around file operations. This ensures file locking
    and the model sees LSP diagnostics immediately.

    ```bash
    gemini extensions install https://github.com/MarkWells-Dev/Catenary
    ```

2.  **Constrained mode.** Use the Policy Engine to deny text-scanning commands while
    keeping Gemini's native file I/O and shell tools available. Create the file
    `~/.gemini/policies/catenary-constrained.toml`:

```toml
# Catenary constrained mode — forces LSP-first navigation
# Place in ~/.gemini/policies/catenary-constrained.toml

# --- 1. Search (Grep Family) ---
[[rule]]
toolName = "run_shell_command"
commandPrefix = [
  "rg", "ag", "ack", "fd",
  "grep", "egrep", "fgrep", "rgrep", "zgrep",
  "git grep",
]
decision = "deny"
priority = 900
deny_message = "Use Catenary's search tool instead."

# --- 2. Navigation (Listing Family) ---
[[rule]]
toolName = "run_shell_command"
commandPrefix = [
  "ls", "dir", "vdir", "tree", "find",
  "locate", "mlocate", "whereis", "which",
  "git ls-files", "git ls-tree",
]
decision = "deny"
priority = 900
deny_message = "Use Catenary's list_directory tool instead."

# --- 3. Peeking (Reading Family) ---
[[rule]]
toolName = "run_shell_command"
commandPrefix = [
  "cat", "head", "tail", "more", "less", "nl",
  "od", "hexdump", "xxd", "strings", "dd", "tee",
]
decision = "deny"
priority = 900
deny_message = "Use the native read_file tool instead."

# --- 4. Text Processing (Scripting Family) ---
[[rule]]
toolName = "run_shell_command"
commandPrefix = [
  "awk", "sed", "perl",
  "cut", "paste", "sort", "uniq", "join",
]
decision = "deny"
priority = 900
deny_message = "Text processing commands are not allowed in constrained mode."

# --- 5. Reconnaissance (Metadata Family) ---
[[rule]]
toolName = "run_shell_command"
commandPrefix = ["file", "stat", "du", "df"]
decision = "deny"
priority = 900
deny_message = "Metadata commands are not allowed in constrained mode."

# --- 6. Executors & Shells (The Wrapper Family) ---
[[rule]]
toolName = "run_shell_command"
commandPrefix = [
  "bash", "sh", "zsh", "dash", "fish",
  "ash", "csh", "ksh", "tcsh",
]
decision = "deny"
priority = 900
deny_message = "Shell wrappers are not allowed in constrained mode."

# --- 7. The Command Runners (Prevents Masquerading) ---
[[rule]]
toolName = "run_shell_command"
commandPrefix = [
  "env", "sudo", "su", "nohup", "timeout", "watch", "time",
  "eval", "exec", "command", "builtin", "type", "hash",
]
decision = "deny"
priority = 900
deny_message = "Command runners are not allowed in constrained mode."

# --- 8. The Multiplexers ---
[[rule]]
toolName = "run_shell_command"
commandPrefix = ["xargs", "parallel"]
decision = "deny"
priority = 900
deny_message = "Multiplexers are not allowed in constrained mode."

# --- 9. Framework Tool Blocks ---
[[rule]]
toolName = "grep_search"
decision = "deny"
priority = 900
deny_message = "Use Catenary's search tool instead."

[[rule]]
toolName = "glob"
decision = "deny"
priority = 900
deny_message = "Use Catenary's list_directory tool instead."

[[rule]]
toolName = "read_many_files"
decision = "deny"
priority = 900
deny_message = "Use Catenary's LSP tools for code navigation."

[[rule]]
toolName = "list_directory"
decision = "deny"
priority = 900
deny_message = "Use Catenary's list_directory tool instead."
```

Then add the MCP server to `.gemini/settings.json`:

```json
{
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
`Write`, and `Bash` tools but deny text-scanning commands to force LSP-first
navigation. This deny list blocks grep, file listing, manual reads, text
processing, shell wrappers, and framework tools that would bypass Catenary.

```json
{
  "permissions": {
    "allow": [
      "WebSearch",
      "WebFetch",
      "mcp__catenary__*",
      "mcp__plugin_catenary_catenary__*",
      "ToolSearch",
      "AskUserQuestion",
      "Bash"
    ],
    "deny": [
      "// --- 1. Search (Grep Family) ---",
      "Bash(rg *)",
      "Bash(ag *)",
      "Bash(ack *)",
      "Bash(fd *)",
      "Bash(grep *)",
      "Bash(egrep *)",
      "Bash(fgrep *)",
      "Bash(rgrep *)",
      "Bash(zgrep *)",
      "Bash(git grep *)",
      "// --- 2. Navigation (Listing Family) ---",
      "Bash(ls *)",
      "Bash(dir *)",
      "Bash(vdir *)",
      "Bash(tree *)",
      "Bash(find *)",
      "Bash(locate *)",
      "Bash(mlocate *)",
      "Bash(whereis *)",
      "Bash(which *)",
      "Bash(git ls-files *)",
      "Bash(git ls-tree *)",
      "// --- 3. Peeking (Reading Family) ---",
      "Bash(cat *)",
      "Bash(head *)",
      "Bash(tail *)",
      "Bash(more *)",
      "Bash(less *)",
      "Bash(nl *)",
      "Bash(od *)",
      "Bash(hexdump *)",
      "Bash(xxd *)",
      "Bash(strings *)",
      "Bash(dd *)",
      "Bash(tee *)",
      "// --- 4. Text Processing (Scripting Family) ---",
      "Bash(awk *)",
      "Bash(sed *)",
      "Bash(perl *)",
      "Bash(cut *)",
      "Bash(paste *)",
      "Bash(sort *)",
      "Bash(uniq *)",
      "Bash(join *)",
      "// --- 5. Reconnaissance (Metadata Family) ---",
      "Bash(file *)",
      "Bash(stat *)",
      "Bash(du *)",
      "Bash(df *)",
      "// --- 6. Executors & Shells (The Wrapper Family) ---",
      "Bash(bash *)",
      "Bash(sh *)",
      "Bash(zsh *)",
      "Bash(dash *)",
      "Bash(fish *)",
      "Bash(ash *)",
      "Bash(csh *)",
      "Bash(ksh *)",
      "Bash(tcsh *)",
      "// --- 7. The Command Runners (Prevents Masquerading) ---",
      "Bash(env *)",
      "Bash(sudo *)",
      "Bash(su *)",
      "Bash(nohup *)",
      "Bash(timeout *)",
      "Bash(watch *)",
      "Bash(time *)",
      "Bash(eval *)",
      "Bash(exec *)",
      "Bash(command *)",
      "Bash(builtin *)",
      "Bash(type *)",
      "Bash(hash *)",
      "// --- 8. The Multiplexers ---",
      "Bash(xargs *)",
      "Bash(parallel *)",
      "// --- 9. Framework Blocks ---",
      "Grep",
      "Glob",
      "Task"
    ]
  },
  "mcpServers": {
    "catenary": {
      "command": "catenary"
    }
  }
}
```

This keeps `Bash` available for build/test/git commands while blocking every
path that would let the model fall back to text scanning. The model uses:

- **Catenary LSP tools** for navigation (`search`, `hover`, `definition`, etc.)
- **Catenary `list_directory`** for directory browsing (replaces `ls`, `tree`, `find`)
- **Claude Code `Read`/`Edit`/`Write`** for file I/O (with `catenary release` hook for diagnostics)
- **Claude Code `Bash`** for build, test, and git commands only

## Experiment Results

### Current: Policy Engine (Gemini) + Deny List (Claude)

Validated 2026-02-17.

| Test                     | Gemini CLI                | Claude Code                                 |
| ------------------------ | ------------------------- | ------------------------------------------- |
| Restriction method       | Policy Engine (`deny`)    | `permissions.deny` list + block `Grep/Glob/Task` |
| MCP tools discovered     | ✓                         | ✓                                           |
| Text scanning blocked    | ✓                         | ✓                                           |
| Model adapts gracefully  | ✓ (immediately)           | ✓ (immediately)                             |
| Sub-agent escape blocked | N/A                       | ✓ (requires denying `Task`)                 |

The policy engine approach gives models clear feedback on *why* a tool is
blocked and *what to use instead* (via `deny_message`). This eliminates the
thrashing seen with earlier approaches — models go straight to Catenary tools
on the first turn without attempting workarounds.

Tested with `gemini-3-flash-preview` and `claude-opus-4-6`. Both adapted
on the first prompt with zero fallback attempts.

### Historical: `tools.core` Allowlist (Gemini, deprecated)

Validated 2026-02-06.

The original Gemini approach used `tools.core` to allowlist only non-file
tools (`web_fetch`, `google_web_search`, `save_memory`), hiding all built-in
file and shell tools. This worked but models adapted slowly — Gemini would
try several workarounds (WebFetch for local files, sub-agent delegation)
before settling on Catenary tools. The policy engine approach replaced this
by giving explicit deny messages instead of silently removing tools.

## Catenary Tool Coverage

Catenary provides LSP intelligence and directory browsing:

| Tool                | Category  | Notes                                    |
| ------------------- | --------- | ---------------------------------------- |
| `list_directory`    | File I/O  | Files, dirs, symlinks                    |
| `search`            | LSP       | Workspace symbols + grep fallback        |
| `find_references`   | LSP       | LSP references                           |
| `codebase_map`      | LSP       | File tree with symbols                   |
| `document_symbols`  | LSP       | File structure                           |
| `hover`             | LSP       | Type info, docs                          |
| `diagnostics`       | LSP       | Errors, warnings                         |
| ...                 | LSP       | [Full list](overview.md#available-tools) |

File I/O is handled by the host tool's native file operations. Catenary
provides post-edit diagnostics via the `catenary release` hook.

## Limitations

### LSP Dependency

Some operations require LSP:

- Find references (no grep fallback currently)
- Rename symbol
- Code actions

If LSP is unavailable for a language, these tools return errors. `search` has
a grep fallback for basic text matching when no LSP server covers the file.

## See Also

- [Archive: CLI Design](archive/cli-design.md) — Original custom CLI design
  (abandoned)
- [Configuration](configuration.md) — catenary-mcp configuration reference

# AI Agent Integration

This guide helps AI coding assistants use Catenary effectively. The goal is to
reduce context bloat and token usage by using semantic LSP queries instead of
text-based file scanning.

## System Prompt

In **constrained mode** (text-scanning commands denied via permissions), add
the following to your system prompt or agent instructions to prevent the model
from wasting tokens discovering the deny list through trial and error:

```
Text-scanning shell commands (grep, find, ls, cat, etc.) are denied.
Use Catenary's grep tool for search and glob tool for browsing.
Workarounds will be added to the deny list.
```

If Catenary is running **alongside built-in tools**, agents will default to
what they were trained on (reading files, grepping). Adding the following to
your system prompt nudges them toward LSP queries instead:

```
## Catenary (LSP Tools)

When exploring or navigating code, prefer Catenary's LSP tools over text search:

| Task | Use | Instead of |
|------|-----|------------|
| Find symbols, references, text matches | `grep` | grep/ripgrep |
| Browse files and directories | `glob` | ls/find/tree |
| Check for errors after edits | hooks | Running compiler |

### Why This Matters

- A single 500-line file read costs ~2000-4000 tokens
- A `grep` call costs ~200-500 tokens and returns symbols + references + text matches
- One file read ≈ 10-20 targeted LSP queries
- Reducing unnecessary reads prevents context compression and re-reads

### When to Still Use Read/Grep

- Understanding implementation logic (not just signatures)
- Searching comments or string literals
- Config files or non-code content
- Small files where full context is needed
```

---

## The Problem

AI agents typically explore codebases by:

1. Running `grep` or similar to find text matches
2. Reading entire files to understand context
3. Repeating this as context windows fill and compress

This creates a "token tax": files are read, forgotten during compression, then
re-read. Each cycle costs tokens and risks hitting rate limits mid-task.

## The Solution

Catenary provides LSP-backed tools that return precise, targeted information.
Instead of reading a 500-line file to find a function's type signature, ask the
language server directly.

## When to Use LSP vs Native File Tools

Catenary provides two MCP tools (`grep` and `glob`) plus post-edit diagnostics
via hooks. File reading and editing is handled by the host tool's native file
operations (e.g. Claude Code's `Read`, `Edit`, `Write`). The `catenary notify`
hook provides post-edit LSP diagnostics so you immediately see any errors
introduced by changes.

**Use Catenary tools** for:

- Finding symbols, references, and text matches (`grep`)
- Browsing files and directory structure (`glob`)
- Post-edit diagnostics (automatic via hooks)

**Use native file tools** for:

- Reading implementation logic (not just signatures)
- Searching comments or string literals (`grep` includes a file heatmap)
- Config files or non-code content
- Writing and editing code (diagnostics returned via notify hook)

## Workflow Example

**Task:** "Fix the bug in the authentication handler"

**Inefficient approach:**

1. Grep for "auth" - returns 50 matches across 20 files
2. Read 5 files looking for the handler
3. Read 3 more files to understand the types involved
4. Context fills up, compression triggers
5. Re-read files to remember what you learned

**Efficient approach:**

1. `grep` for "auth" — returns symbols, semantic references, and text matches
2. `grep` for the specific handler — definition and type info included in results
3. Read the specific function you need to modify
4. Edit to make the change — diagnostics returned via notify hook

## Codebase Orientation

When first exploring an unfamiliar codebase:

```
# Get project structure with function/class names
glob with pattern: "**" and include_symbols: true

# Then drill down with targeted queries
grep for specific components

# Read implementation when needed
Read the specific code you need to understand
```

This provides a mental map without reading every file.

## Token Efficiency Comparison

Typical token costs (approximate):

| Operation                             | Tokens     |
| ------------------------------------- | ---------- |
| Read a 500-line file                  | ~2000-4000 |
| `grep` (symbols + references + heatmap) | ~200-500 |
| `glob` (file listing)                | ~200-800   |
| `glob` with symbols (budget: 200)    | ~800-1000  |

A single file read can cost as much as 10-20 targeted LSP queries.

## Key Principles

1. **Ask, don't scan.** If you have a specific question ("where is X defined?"),
   use a targeted LSP query.

2. **Structure before content.** Use `glob` with symbols to understand
   organization before reading implementation.

3. **Search before read.** Use `grep` to find symbols, references, and
   matches before reading source files.

4. **References are precise.** `grep` enriches results with semantic
   references — actual usages, not text matches.

5. **Save reads for logic.** Only read files when you need to understand
   _how_ something works, not _what_ it is or _where_ it lives.

6. **Edit with feedback.** The `catenary notify` hook returns LSP diagnostics
   after every edit, so you immediately see any errors introduced.

## Notify Hook

Catenary provides post-edit LSP diagnostics via the `catenary notify` command,
designed for use as a `PostToolUse` hook in Claude Code.

The recommended setup uses the Catenary plugin (`catenary@catenary`), which
registers the `catenary notify` hook automatically. For manual configuration,
add to `.claude/settings.json`:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Edit|Write|NotebookEdit|Read",
        "hooks": [
          {
            "type": "command",
            "command": "catenary notify --format=claude"
          }
        ]
      }
    ]
  }
}
```

The `notify` hook reads the `PostToolUse` JSON from stdin, finds the running
Catenary session for the workspace, and returns LSP diagnostics. It exits
silently on any error so it never blocks the host tool's flow.

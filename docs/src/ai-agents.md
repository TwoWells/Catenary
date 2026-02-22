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
Use Catenary's LSP tools for navigation and list_directory for browsing.
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
| Find where something is defined | `definition` | grep/ripgrep |
| Find all usages of a symbol | `find_references` | grep/ripgrep |
| Get type info or documentation | `hover` | Reading entire files |
| Understand a file's structure | `document_symbols` | Reading entire files |
| Find a class/function by name | `search` | grep/glob patterns |
| See available methods on an object | `completion` | Reading class definitions |
| Find implementations of interface | `implementation` | grep for impl blocks |
| Rename a symbol safely | `rename` | Find/replace with grep |
| Check for errors after edits | `diagnostics` | Running compiler |
| Explore unfamiliar codebase | `codebase_map` | Multiple grep/read cycles |

### Why This Matters

- A single 500-line file read costs ~2000-4000 tokens
- An `hover` call costs ~50-200 tokens
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

Catenary provides LSP tools and `list_directory`. File reading and editing is
handled by the host tool's native file operations (e.g. Claude Code's `Read`,
`Edit`, `Write`). The `catenary release` hook provides post-edit LSP diagnostics
so you immediately see any errors introduced by changes.

**Use LSP tools** for:

- Finding definitions, references, and symbols
- Getting type info and documentation (hover)
- Understanding file structure (document_symbols)
- Checking errors after changes (diagnostics)

**Use native file tools** for:

- Reading implementation logic (not just signatures)
- Searching comments or string literals (`search` includes a file heatmap)
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

1. `search` for "auth" - returns symbol names with locations
2. `definition` to jump to the specific handler
3. `hover` on unfamiliar types to understand them
4. `find_references` to see how the handler is called
5. Read the specific function you need to modify
6. Edit to make the change — diagnostics returned via notify hook

## Codebase Orientation

When first exploring an unfamiliar codebase:

```
# Get project structure with function/class names
codebase_map with include_symbols: true

# Then drill down with targeted queries
search for specific components
document_symbols for file structure

# Read implementation when needed
Read the specific code you need to understand
```

This provides a mental map without reading every file.

## Token Efficiency Comparison

Typical token costs (approximate):

| Operation                             | Tokens     |
| ------------------------------------- | ---------- |
| Read a 500-line file                  | ~2000-4000 |
| `hover` response                  | ~50-200    |
| `definition` response             | ~30-100    |
| `find_references` (10 results) | ~200-500   |
| `document_symbols`                | ~200-800   |
| `codebase_map` (budget: 200) | ~800-1000  |

A single file read can cost as much as 10-20 targeted LSP queries.

## Key Principles

1. **Ask, don't scan.** If you have a specific question ("where is X defined?"),
   use a targeted LSP query.

2. **Structure before content.** Use `document_symbols` or `codebase_map` to
   understand organization before reading implementation.

3. **Hover before read.** Check `hover` for type signatures and docs before
   reading source files.

4. **References are precise.** `find_references` finds actual usages,
   not text matches. No false positives from comments or strings.

5. **Save reads for logic.** Only read files when you need to understand
   _how_ something works, not _what_ it is or _where_ it lives.

6. **Edit with feedback.** The `catenary release` hook returns LSP diagnostics
   after every edit, so you immediately see any errors introduced.

## Release Hook

Catenary provides post-edit LSP diagnostics, mtime tracking, and lock release
via the `catenary release` command, designed for use as a `PostToolUse` hook
in Claude Code.

The recommended setup uses the Catenary plugin (`catenary@catenary`), which
registers `catenary acquire` / `catenary release` hooks automatically. For
manual configuration, add to `.claude/settings.json`:

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
  }
}
```

The `release` hook reads the `PostToolUse` JSON from stdin, finds the running
Catenary session for the workspace, runs LSP diagnostics, records the file's
mtime, and releases the file lock. It exits silently on any error so it never
blocks the host tool's flow.

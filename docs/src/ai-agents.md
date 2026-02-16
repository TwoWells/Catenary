# AI Agent Integration

This guide helps AI coding assistants use Catenary effectively. The goal is to
reduce context bloat and token usage by using semantic LSP queries instead of
text-based file scanning.

## System Prompt

In **constrained mode** (built-in file/shell tools disabled), the agent has no
choice but to use Catenary's LSP tools — no system prompt changes are needed.

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

## When to Use LSP vs File I/O

Catenary provides both LSP tools and file I/O tools (`read_file`, `write_file`,
`edit_file`, `list_directory`). Use LSP tools for navigation and understanding;
use file I/O for reading implementation logic and making changes.

**Use LSP tools** for:

- Finding definitions, references, and symbols
- Getting type info and documentation (hover)
- Understanding file structure (document_symbols)
- Checking errors after changes (diagnostics)

**Use file I/O tools** for:

- Reading implementation logic (not just signatures)
- Searching comments or string literals (use `search` with grep fallback)
- Config files or non-code content
- Writing and editing code (`write_file`, `edit_file` return diagnostics
  automatically)

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
5. `read_file` on the specific function you need to modify
6. `edit_file` to make the change — diagnostics returned automatically

## Codebase Orientation

When first exploring an unfamiliar codebase:

```
# Get project structure with function/class names
codebase_map with include_symbols: true

# Then drill down with targeted queries
search for specific components
document_symbols for file structure

# Read implementation when needed
read_file for the specific code you need to understand
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

5. **Save reads for logic.** Only use `read_file` when you need to understand
   _how_ something works, not _what_ it is or _where_ it lives.

6. **Edit with feedback.** Use `edit_file` and `write_file` — they return LSP
   diagnostics automatically, so you immediately see any errors introduced.

## Display Hooks

Catenary's `edit_file` and `write_file` tools pass raw JSON parameters to the
CLI, which can be hard to review. The bundled hook script formats these as
colorized diffs and previews.

### Claude Code

Copy the script and make it executable:

```bash
mkdir -p ~/.claude/hooks
cp .claude-plugin/plugins/catenary/hooks/format_tool_output.py ~/.claude/hooks/
chmod +x ~/.claude/hooks/format_tool_output.py
```

Add to `~/.claude/settings.json` (all projects) or `.claude/settings.json`
(single project):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "mcp__.*__edit_file|mcp__.*__write_file",
        "hooks": [
          {
            "type": "command",
            "command": "~/.claude/hooks/format_tool_output.py"
          }
        ]
      }
    ]
  }
}
```

### Gemini CLI

Not currently supported. Gemini CLI's `BeforeTool` hook fires *after* the user
approves the tool call, so there's no way to show a formatted diff in the
approval prompt. The hook output only appears in the debug console, not the
main UI.

### What you get

- **edit_file**: colorized unified diff (red = removed, green = added)
- **write_file**: file header with line count and first 30 lines numbered

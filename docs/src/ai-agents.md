# AI Agent Integration

This guide helps AI coding assistants use Catenary effectively. The goal is to
reduce context bloat and token usage by using semantic LSP queries instead of
text-based file scanning.

## System Prompt

Consider adding some or all of the following to your system prompt (e.g.,
`CLAUDE.md`, `GEMINI.md`, `.github/copilot-instructions.md`):

```
## Catenary (LSP Tools)

When exploring or navigating code, prefer Catenary's LSP tools over text search:

| Task | Use | Instead of |
|------|-----|------------|
| Find where something is defined | `lsp_definition` | grep/ripgrep |
| Find all usages of a symbol | `catenary_find_references` | grep/ripgrep |
| Get type info or documentation | `lsp_hover` | Reading entire files |
| Understand a file's structure | `lsp_document_symbols` | Reading entire files |
| Find a class/function by name | `catenary_find_symbol` | grep/glob patterns |
| See available methods on an object | `lsp_completion` | Reading class definitions |
| Find implementations of interface | `lsp_implementation` | grep for impl blocks |
| Rename a symbol safely | `lsp_rename` (with `dry_run: true`) | Find/replace with grep |
| Check for errors after edits | `lsp_diagnostics` | Running compiler |
| Explore unfamiliar codebase | `catenary_codebase_map` | Multiple grep/read cycles |

### Why This Matters

- A single 500-line file read costs ~2000-4000 tokens
- An `lsp_hover` call costs ~50-200 tokens
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

## Limitations

Catenary doesn't replace file reading entirely. Agents will still need Read/Grep
when:

- Understanding implementation logic (not just signatures)
- Searching for patterns in comments or string literals
- Looking for configuration values or constants
- Working with content that lacks LSP support (plain text, logs, etc.)

## Workflow Example

**Task:** "Fix the bug in the authentication handler"

**Inefficient approach:**

1. Grep for "auth" - returns 50 matches across 20 files
2. Read 5 files looking for the handler
3. Read 3 more files to understand the types involved
4. Context fills up, compression triggers
5. Re-read files to remember what you learned

**Efficient approach:**

1. `catenary_find_symbol` for "auth" - returns symbol names with locations
2. `lsp_definition` to jump to the specific handler
3. `lsp_hover` on unfamiliar types to understand them
4. `catenary_find_references` to see how the handler is called
5. Only `Read` the specific function you need to modify

## Codebase Orientation

When first exploring an unfamiliar codebase:

```
# Get project structure with function/class names
catenary_codebase_map with include_symbols: true

# Then drill down with targeted queries
catenary_find_symbol for specific components
lsp_document_symbols for file structure
```

This provides a mental map without reading every file.

## Token Efficiency Comparison

Typical token costs (approximate):

| Operation                             | Tokens     |
| ------------------------------------- | ---------- |
| Read a 500-line file                  | ~2000-4000 |
| `lsp_hover` response                  | ~50-200    |
| `lsp_definition` response             | ~30-100    |
| `catenary_find_references` (10 results) | ~200-500   |
| `lsp_document_symbols`                | ~200-800   |
| `catenary_codebase_map` (budget: 200) | ~800-1000  |

A single file read can cost as much as 10-20 targeted LSP queries.

## Key Principles

1. **Ask, don't scan.** If you have a specific question ("where is X defined?"),
   use a targeted LSP query.

2. **Structure before content.** Use `document_symbols` or `codebase_map` to
   understand organization before reading implementation.

3. **Hover before read.** Check `lsp_hover` for type signatures and docs before
   reading source files.

4. **References are precise.** `catenary_find_references` finds actual usages,
   not text matches. No false positives from comments or strings.

5. **Save reads for logic.** Only read files when you need to understand _how_
   something works, not _what_ it is or _where_ it lives.

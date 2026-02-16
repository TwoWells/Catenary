# Overview

## The Problem

AI coding agents navigate code by reading files and grepping for patterns.
This works, but it's wasteful.

Context windows are **append-only**. Every file the agent reads, every edit it
makes, every verification read — all of it accumulates. A single 500-line file
read-edited-verified three times puts three full copies into context. Every
token in that growing context is re-processed on every subsequent turn.

In practice, this creates a massive amplification effect. A few hours of work
can produce over **100 million tokens** of re-processed context, even though
the developer only typed a few thousand tokens of instructions. Most of that
is the model re-reading the same file contents over and over.

Bigger context windows don't fix this. They let you be wasteful for longer
before hitting the wall, but every token still costs compute and latency on
every turn. The problem scales with session length, not window size.

## The Solution

Catenary replaces brute-force file scanning with **graph navigation**.

Instead of reading a 500-line file to find a type signature, the agent asks
the language server directly — `hover` returns 50 tokens instead of 2,000.
Instead of grepping across 20 files to find a definition, `definition` returns
the exact location in one query. Instead of re-reading a file after editing it
to check for errors, `edit_file` returns diagnostics inline.

Each LSP query is small and stateless. Nothing accumulates. The context stays
lean across the entire session, regardless of how long the agent works.

| Brute force | Tokens | Context cost |
|-------------|--------|--------------|
| Read file to find type info | ~2,000 | +1 copy |
| Read file again after edit | ~2,000 | +1 copy (2 total) |
| Grep 20 files for a definition | ~8,000 | +20 partial copies |

| Graph navigation | Tokens | Context cost |
|------------------|--------|--------------|
| `hover` for type info | ~100 | stateless |
| `edit_file` (diagnostics included) | ~300 | no re-read |
| `definition` | ~50 | stateless |

## How It Works

```
┌─────────────┐     MCP      ┌──────────┐     LSP      ┌─────────────────┐
│ AI Assistant│◄────────────►│ Catenary │◄────────────►│ Language Server │
│ (Claude)    │              │          │              │ (rust-analyzer) │
└─────────────┘              │          │◄────────────►│ (pyright)       │
                             │          │              │ (gopls)         │
                             └──────────┘              └─────────────────┘
```

Catenary bridges [MCP](https://modelcontextprotocol.io/) and
[LSP](https://microsoft.github.io/language-server-protocol/). It manages
multiple language servers, routes requests by file type, and provides file I/O
with automatic diagnostics — all through a single MCP server. The agent never
needs to know which server handles which language.

## Constrained Mode

Catenary is designed to be the agent's **primary toolkit**, not a supplement.
When you disable the host CLI's built-in file and shell tools, the agent is
forced to use LSP queries for navigation and Catenary's file I/O for edits.
This eliminates the fallback to brute-force reads and keeps context usage
minimal.

See [CLI Integration](cli-integration.md) for setup instructions.

> Catenary also works as a supplement alongside built-in tools. But without
> constraints, agents default to what they were trained on — reading files and
> grepping — and the efficiency gains are lost.

## Features

| Feature               | Description                                                                            |
| --------------------- | -------------------------------------------------------------------------------------- |
| **LSP Multiplexing**  | Run multiple language servers in a single Catenary instance                            |
| **Lazy Loading**      | Servers only start when you open a file of that language                               |
| **Smart Routing**     | Requests automatically route to the correct server based on file type                  |
| **Universal Support** | Works with any LSP-compliant language server                                           |
| **Full LSP Coverage** | Hover, definitions, references, diagnostics, completions, rename, formatting, and more |
| **File I/O**          | Read, write, and edit files with automatic LSP diagnostics                            |
| **Shell Execution**   | Run commands with configurable allowlists and language detection                       |

## Available Tools

### LSP Tools

| Tool                      | Description                                         |
| ------------------------- | --------------------------------------------------- |
| `hover`               | Get documentation and type info for a symbol        |
| `definition`          | Jump to where a symbol is defined                   |
| `type_definition`     | Jump to the type's definition                       |
| `implementation`      | Find implementations of interfaces/traits           |
| `find_references` | Find all references to a symbol (by name or position) |
| `document_symbols`    | Get the outline of a file (supports `wait_for_reanalysis: true`) |
| `search`         | Search for a symbol or pattern (LSP with grep fallback) |
| `code_actions`        | Get quick fixes and refactorings                    |
| `rename`              | Compute rename edits (does not modify files)        |
| `completion`          | Get completion suggestions                          |
| `signature_help`      | Get function parameter info                         |
| `diagnostics`         | Get errors and warnings (supports `wait_for_reanalysis: true` to ensure fresh results) |
| `formatting`          | Format a document                                   |
| `range_formatting`    | Format a selection                                  |
| `call_hierarchy`      | See who calls a function / what it calls            |
| `type_hierarchy`      | See type inheritance                                |
| `status`         | Report status of all LSP servers (e.g. "Indexing")  |
| `apply_quickfix` | Find a quick fix and return its proposed edits       |
| `codebase_map`   | Generate a high-level file tree with symbols        |

### File I/O Tools

| Tool                      | Description                                         |
| ------------------------- | --------------------------------------------------- |
| `read_file`          | Read file contents with line numbers and diagnostics |
| `write_file`         | Write content to a file, returns diagnostics         |
| `edit_file`          | Search-and-replace edit, returns diagnostics         |
| `list_directory`     | List directory contents (files, dirs, symlinks)      |

Write and edit tools **automatically return LSP diagnostics** after modifying
files, so the model immediately sees any errors introduced by its changes.

All file paths are validated against workspace roots. Catenary's own
configuration files are protected from modification.

### Shell Execution

| Tool                      | Description                                         |
| ------------------------- | --------------------------------------------------- |
| `run`                | Execute a shell command (allowlist enforced)          |

The `run` tool requires explicit configuration. See
[Configuration](configuration.md#shell-execution-toolsrun) for setup.

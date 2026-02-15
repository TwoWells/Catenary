# Overview

Catenary is a bridge between **MCP** (Model Context Protocol) and **LSP**
(Language Server Protocol).

It allows AI coding assistants to access the same code intelligence that powers
your IDE — accurate, real-time information straight from language servers.

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

## How It Works

```
┌─────────────┐     MCP      ┌──────────┐     LSP      ┌─────────────────┐
│ AI Assistant│◄────────────►│ Catenary │◄────────────►│ Language Server │
│ (Claude)    │              │          │              │ (rust-analyzer) │
└─────────────┘              │          │◄────────────►│ (pyright)       │
                             │          │              │ (gopls)         │
                             └──────────┘              └─────────────────┘
```

Catenary translates MCP tool calls into LSP requests, routes them to the
appropriate language server, and returns the results. The AI never needs to know
which server handles which language.

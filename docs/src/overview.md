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

## Available Tools

Once connected, your AI assistant gains access to these LSP-powered tools:

| Tool                      | Description                                         |
| ------------------------- | --------------------------------------------------- |
| `hover`               | Get documentation and type info for a symbol        |
| `definition`          | Jump to where a symbol is defined                   |
| `type_definition`     | Jump to the type's definition                       |
| `implementation`      | Find implementations of interfaces/traits           |
| `find_references` | Find all references to a symbol (by name or position) |
| `document_symbols`    | Get the outline of a file (supports `wait_for_reanalysis: true`) |
| `find_symbol`    | Find a symbol by name (with fallback for private symbols) |
| `code_actions`        | Get quick fixes and refactorings                    |
| `rename`              | Rename a symbol (with dry-run preview)              |
| `completion`          | Get completion suggestions                          |
| `signature_help`      | Get function parameter info                         |
| `diagnostics`         | Get errors and warnings (supports `wait_for_reanalysis: true` to ensure fresh results) |
| `formatting`          | Format a document                                   |
| `range_formatting`    | Format a selection                                  |
| `call_hierarchy`      | See who calls a function / what it calls            |
| `type_hierarchy`      | See type inheritance                                |
| `status`         | Report status of all LSP servers (e.g. "Indexing")  |
| `apply_quickfix` | Automatically find and apply a fix for a diagnostic |
| `codebase_map`   | Generate a high-level file tree with symbols        |

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

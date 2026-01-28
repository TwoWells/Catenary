# Catenary

[![CI](https://github.com/Mark-Wells-Dev/Catenary/actions/workflows/ci.yml/badge.svg)](https://github.com/Mark-Wells-Dev/Catenary/actions/workflows/ci.yml)

A bridge between [MCP](https://modelcontextprotocol.io/) (Model Context Protocol) and [LSP](https://microsoft.github.io/language-server-protocol/) (Language Server Protocol).

Catenary allows LLM-powered tools to access IDE-quality code intelligence by exposing LSP capabilities as MCP tools.

📚 **[Documentation Wiki](https://github.com/Mark-Wells-Dev/Catenary/wiki)** — Setup guides for 20+ language servers

## Features

- **LSP Multiplexing** - Run multiple language servers (Rust, Python, Go, etc.) in a single Catenary instance
- **Lazy Loading** - Servers only start when you open a file of that language
- **Smart Routing** - Automatically routes requests to the correct LSP server based on file extension
- **Universal LSP support** - Works with any LSP server (rust-analyzer, gopls, pyright, typescript-language-server, etc.)
- **Full LSP coverage** - Hover, go-to-definition, find references, completions, diagnostics, rename, formatting, and more
- **Smart Encoding** - Automatically negotiates UTF-8 position encoding for accurate emoji and multi-byte character support
- **MCP-native** - Exposes all features as MCP tools with proper schemas

## Installation

```bash
cargo install catenary-mcp
```

Or build from source:

```bash
git clone https://github.com/Mark-Wells-Dev/Catenary
cd catenary
cargo build --release
```

## Configuration

Catenary reads language server definitions from `~/.config/catenary/config.toml`:

```toml
idle_timeout = 300  # Seconds before closing idle documents (0 to disable)

[server.rust]
command = "rust-analyzer"

[server.python]
command = "pyright-langserver"
args = ["--stdio"]

[server.typescript]
command = "typescript-language-server"
args = ["--stdio"]

[server.javascript]
command = "typescript-language-server"
args = ["--stdio"]

[server.go]
command = "gopls"

[server.c]
command = "clangd"

[server.cpp]
command = "clangd"

[server.shellscript]
command = "bash-language-server"
args = ["start"]

[server.yaml]
command = "yaml-language-server"
args = ["--stdio"]

[server.toml]
command = "taplo"
args = ["lsp", "stdio"]
```

See [`plugins/catenary/config.example.toml`](plugins/catenary/config.example.toml) for a complete example with many languages.

**Note:** Catenary uses lazy loading - servers only start when needed, so add as many as you like without overhead.

### CLI Override

You can also specify servers via CLI (useful for testing or one-off use):

```bash
catenary --lsp "rust:rust-analyzer" --lsp "python:pyright-langserver --stdio"
```

CLI arguments append to (and override) the config file.

## Available MCP Tools

| Tool                    | Description                                                   |
| ----------------------- | ------------------------------------------------------------- |
| `lsp_hover`             | Get hover information (documentation, type info) for a symbol |
| `lsp_definition`        | Go to the definition of a symbol                              |
| `lsp_type_definition`   | Go to the type definition of a symbol                         |
| `lsp_implementation`    | Find implementations of an interface or trait                 |
| `lsp_references`        | Find all references to a symbol                               |
| `lsp_document_symbols`  | Get the symbol outline of a file                              |
| `lsp_workspace_symbols` | Search for symbols across the workspace                       |
| `lsp_code_actions`      | Get available code actions (quick fixes, refactorings)        |
| `lsp_rename`            | Rename a symbol (supports dry run or applying to disk)        |
| `lsp_completion`        | Get completion suggestions at a position                      |
| `lsp_signature_help`    | Get function signature help                                   |
| `lsp_diagnostics`       | Get diagnostics (errors, warnings) for a file                 |
| `lsp_formatting`        | Format a document                                             |
| `lsp_range_formatting`  | Format a range within a document                              |
| `lsp_call_hierarchy`    | Get incoming/outgoing calls for a function                    |
| `lsp_type_hierarchy`    | Get supertypes/subtypes of a type                             |

## MCP Client Setup

Once you've [configured your language servers](#configuration), add Catenary to your MCP client:

### Claude Code (CLI)

**Option 1: Plugin (recommended)**

```bash
claude plugin marketplace add Mark-Wells-Dev/Catenary
claude plugin install catenary@catenary
```

**Option 2: Manual**

```bash
claude mcp add catenary -- catenary
```

### Claude Desktop

Add to `~/.config/claude/claude_desktop_config.json` (Linux) or `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS):

```json
{
  "mcpServers": {
    "catenary": {
      "command": "catenary"
    }
  }
}
```

### Gemini CLI

Add to `~/.gemini/settings.json`:

```json
{
  "mcpServers": {
    "catenary": {
      "command": "catenary"
    }
  }
}
```

### Generic MCP Client

```json
{
  "mcpServers": {
    "catenary": {
      "command": "catenary"
    }
  }
}
```

## Language Server Guides

See the **[Wiki](https://github.com/Mark-Wells-Dev/Catenary/wiki/LSPs)** for setup guides covering Rust, Python, TypeScript, Go, PHP, CSS/HTML/JSON, Julia, and more.

## License & Commercial

### Open Source
Catenary is free software: you can redistribute it and/or modify it under the terms of the **GNU General Public License as published by the Free Software Foundation**, either version 3 of the License, or (at your option) any later version.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the [GNU General Public License](LICENSE) for more details.

### Commercial Licensing
For organizations that wish to use Catenary in proprietary applications or require features/modifications without the restrictions of the GPL, **Commercial Licenses** are available.

Commercial licensing includes:
- Right to link Catenary libraries into proprietary software.
- Priority support and roadmap influence.
- Custom feature development.

Please contact **Mark Wells Dev** at `contact@markwells.dev` for pricing and details.

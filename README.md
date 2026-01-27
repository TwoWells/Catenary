# Catenary

[![CI](https://github.com/Mark-Wells-Dev/Catenary/actions/workflows/ci.yml/badge.svg)](https://github.com/Mark-Wells-Dev/Catenary/actions/workflows/ci.yml)

A bridge between [MCP](https://modelcontextprotocol.io/) (Model Context Protocol) and [LSP](https://microsoft.github.io/language-server-protocol/) (Language Server Protocol).

Catenary allows LLM-powered tools to access IDE-quality code intelligence by exposing LSP capabilities as MCP tools.

## Features

- **LSP Multiplexing** - Run multiple language servers (Rust, Python, Go, etc.) in a single Catenary instance.
- **Smart Routing** - Automatically routes requests to the correct LSP server based on file extension.
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

## Usage

```bash
catenary --lsp "rust:rust-analyzer" --lsp "shellscript:bash-language-server start" --root /path/to/project
```

### Arguments

- `--lsp, -l` - LSP server specification in `lang:command` format (can be specified multiple times)
- `--root, -r` - Workspace root directory (default: `.`)
- `--idle-timeout` - Seconds before closing idle documents (default: `300`, set to `0` to disable)

### Example: Multiplexing

You can run one Catenary instance that handles all your project's languages:

```bash
catenary \
  --lsp "rust:rust-analyzer" \
  --lsp "python:pyright-langserver --stdio" \
  --lsp "shellscript:bash-language-server start" \
  --root .
```

Catenary will automatically detect the language of the file you are working on and route the LSP request to the appropriate server. For workspace-wide operations (like `lsp_workspace_symbols`), it queries all servers and merges the results.

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

## MCP Configuration

### Claude Desktop

Add to `~/.config/claude/claude_desktop_config.json` (Linux) or `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS):

```json
{
  "mcpServers": {
    "rust-lsp": {
      "command": "catenary",
      "args": ["--command", "rust-analyzer", "--root", "/path/to/rust/project"]
    },
    "python-lsp": {
      "command": "catenary",
      "args": [
        "--command",
        "pyright-langserver --stdio",
        "--root",
        "/path/to/python/project"
      ]
    },
    "typescript-lsp": {
      "command": "catenary",
      "args": [
        "--command",
        "typescript-language-server --stdio",
        "--root",
        "/path/to/ts/project"
      ]
    }
  }
}
```

### Claude Code (CLI)

Add to `~/.claude/settings.json`:

```json
{
  "mcpServers": {
    "lsp": {
      "command": "catenary",
      "args": ["--command", "rust-analyzer"]
    }
  }
}
```

Note: `--root` defaults to the current directory, so it can be omitted when running from the project root.

### Generic MCP Client

```json
{
  "mcpServers": {
    "catenary": {
      "command": "catenary",
      "args": ["--command", "<lsp-command>", "--root", "<workspace-path>"]
    }
  }
}
```

### Common LSP Server Commands

| Language   | Command                              |
| ---------- | ------------------------------------ |
| Rust       | `rust-analyzer`                      |
| Go         | `gopls`                              |
| Python     | `pyright-langserver --stdio`         |
| TypeScript | `typescript-language-server --stdio` |
| C/C++      | `clangd`                             |
| Lua        | `lua-language-server`                |
| Bash       | `bash-language-server start`         |
| YAML       | `yaml-language-server --stdio`       |

## License

GPL-3.0

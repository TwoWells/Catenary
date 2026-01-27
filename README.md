# Catenary

A bridge between [MCP](https://modelcontextprotocol.io/) (Model Context Protocol) and [LSP](https://microsoft.github.io/language-server-protocol/) (Language Server Protocol).

Catenary allows LLM-powered tools to access IDE-quality code intelligence by exposing LSP capabilities as MCP tools.

## Features

- **Universal LSP support** - Works with any LSP server (rust-analyzer, gopls, pyright, typescript-language-server, etc.)
- **Full LSP coverage** - Hover, go-to-definition, find references, completions, diagnostics, rename, formatting, and more
- **MCP-native** - Exposes all features as MCP tools with proper schemas

## Installation

```bash
cargo install catenary
```

Or build from source:

```bash
git clone https://github.com/Mark-Wells-Dev/Catenary
cd catenary
cargo build --release
```

## Usage

```bash
catenary --command "rust-analyzer" --root /path/to/project
```

### Arguments

- `--command, -c` - The LSP server command to spawn (required)
- `--root, -r` - Workspace root directory (default: `.`)
- `--idle-timeout` - Seconds before closing idle documents (default: `300`, set to `0` to disable)

### Examples

```bash
# Rust
catenary --command "rust-analyzer" --root ./my-rust-project

# Go
catenary --command "gopls" --root ./my-go-project

# Python
catenary --command "pyright-langserver --stdio" --root ./my-python-project

# TypeScript
catenary --command "typescript-language-server --stdio" --root ./my-ts-project
```

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
| `lsp_rename`            | Compute edits needed to rename a symbol                       |
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

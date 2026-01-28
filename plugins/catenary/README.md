# Catenary Plugin for Claude Code

Bridge between MCP and LSP, providing IDE-quality code intelligence for Claude Code.

## Features

- **LSP Multiplexing** - Run multiple language servers in a single instance
- **Smart Routing** - Automatically routes requests based on file extension
- **Full LSP Coverage** - Hover, go-to-definition, references, diagnostics, completions, rename, formatting, and more

## Installation

### Via Claude Code Plugin System

```bash
# Add the Catenary marketplace
claude plugin marketplace add github:Mark-Wells-Dev/Catenary

# Install the plugin
claude plugin install catenary@catenary
```

### Prerequisites

Install Catenary and your desired language servers:

```bash
# Install Catenary
cargo install catenary-mcp

# Install language servers (examples)
rustup component add rust-analyzer      # Rust
npm i -g pyright                        # Python
npm i -g typescript-language-server     # TypeScript/JavaScript
go install golang.org/x/tools/gopls@latest  # Go
```

## Default Language Servers

The plugin comes pre-configured with common language servers:

| Language   | Server                       |
| ---------- | ---------------------------- |
| Rust       | rust-analyzer                |
| Python     | pyright-langserver           |
| TypeScript | typescript-language-server   |
| JavaScript | typescript-language-server   |
| Go         | gopls                        |
| C/C++      | clangd                       |

## Customization

To add more language servers or customize the configuration, you can:

1. Edit the plugin's `.mcp.json` after installation
2. Or use Catenary directly via `claude mcp add` with your preferred configuration

### Adding More Languages

Common language server commands:

| Language   | Command                              |
| ---------- | ------------------------------------ |
| Bash       | `bash-language-server start`         |
| YAML       | `yaml-language-server --stdio`       |
| TOML       | `taplo lsp stdio`                    |
| Lua        | `lua-language-server`                |
| Zig        | `zls`                                |
| Haskell    | `haskell-language-server --stdio`    |
| Markdown   | `marksman server`                    |

## Available MCP Tools

Once installed, Claude Code gains access to these LSP-powered tools:

- `lsp_hover` - Get documentation and type info
- `lsp_definition` - Go to definition
- `lsp_references` - Find all references
- `lsp_diagnostics` - Get errors and warnings
- `lsp_completion` - Get completions
- `lsp_rename` - Rename symbols
- `lsp_formatting` - Format code
- And more...

## More Information

- [Catenary Repository](https://github.com/Mark-Wells-Dev/Catenary)
- [Full Documentation](https://github.com/Mark-Wells-Dev/Catenary#readme)

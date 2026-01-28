# Catenary Plugin for Claude Code

Bridge between MCP and LSP, providing IDE-quality code intelligence for Claude Code.

## Features

- **LSP Multiplexing** - Run multiple language servers in a single instance
- **Smart Routing** - Automatically routes requests based on file extension
- **Lazy Loading** - Servers only start when you open a file of that language
- **Full LSP Coverage** - Hover, go-to-definition, references, diagnostics, completions, rename, formatting, and more

## Installation

### 1. Install Catenary

```bash
cargo install catenary-mcp
```

### 2. Install the Plugin

```bash
# Add the Catenary marketplace
claude plugin marketplace add Mark-Wells-Dev/Catenary

# Install the plugin
claude plugin install catenary@catenary
```

### 3. Configure Language Servers

Copy the example config to your config directory:

```bash
mkdir -p ~/.config/catenary
cp ~/.claude/plugins/marketplaces/catenary/plugins/catenary/config.example.toml ~/.config/catenary/config.toml
```

Edit `~/.config/catenary/config.toml` to enable the language servers you need.

### 4. Install Language Servers

Install the language servers for languages you want to use:

```bash
# Rust
rustup component add rust-analyzer

# Python
npm i -g pyright

# TypeScript/JavaScript
npm i -g typescript-language-server typescript

# Go
go install golang.org/x/tools/gopls@latest

# C/C++
# Install clangd via your package manager

# Bash
npm i -g bash-language-server

# YAML
npm i -g yaml-language-server

# TOML
cargo install taplo-cli --locked

# Lua
# Install lua-language-server via your package manager
```

## Configuration

Catenary reads from `~/.config/catenary/config.toml`. Example:

```toml
idle_timeout = 300

[server.rust]
command = "rust-analyzer"

[server.python]
command = "pyright-langserver"
args = ["--stdio"]

[server.typescript]
command = "typescript-language-server"
args = ["--stdio"]

[server.shellscript]
command = "bash-language-server"
args = ["start"]
```

See `config.example.toml` for a full list of common language servers.

**Note:** Catenary uses lazy loading - servers only start when needed, so add as many as you like without overhead.

## Available MCP Tools

Once installed, Claude Code gains access to these LSP-powered tools:

| Tool | Description |
|------|-------------|
| `lsp_hover` | Get documentation and type info |
| `lsp_definition` | Go to definition |
| `lsp_type_definition` | Go to type definition |
| `lsp_implementation` | Find implementations |
| `lsp_references` | Find all references |
| `lsp_document_symbols` | Get file outline |
| `lsp_workspace_symbols` | Search symbols across workspace |
| `lsp_code_actions` | Get quick fixes and refactorings |
| `lsp_rename` | Rename symbols (with dry-run support) |
| `lsp_completion` | Get completions |
| `lsp_diagnostics` | Get errors and warnings |
| `lsp_formatting` | Format code |
| `lsp_call_hierarchy` | Get incoming/outgoing calls |
| `lsp_type_hierarchy` | Get supertypes/subtypes |

## More Information

- [Catenary Repository](https://github.com/Mark-Wells-Dev/Catenary)
- [Full Documentation](https://github.com/Mark-Wells-Dev/Catenary#readme)

# Catenary

[![CI](https://github.com/Mark-Wells-Dev/Catenary/actions/workflows/ci.yml/badge.svg)](https://github.com/Mark-Wells-Dev/Catenary/actions/workflows/ci.yml)

A bridge between [MCP](https://modelcontextprotocol.io/) and [LSP](https://microsoft.github.io/language-server-protocol/) — giving AI coding assistants IDE-quality code intelligence.

📚 **[Documentation](https://github.com/Mark-Wells-Dev/Catenary/wiki)**

## Quick Start

```bash
# Install Catenary
cargo install catenary-mcp

# Add to Claude Code
claude mcp add catenary -- catenary

# Or install as a plugin
claude plugin marketplace add Mark-Wells-Dev/Catenary
claude plugin install catenary@catenary
```

Then [configure your language servers](https://github.com/Mark-Wells-Dev/Catenary/wiki/Config) in `~/.config/catenary/config.toml`.

## Features

- **LSP Multiplexing** — Multiple language servers in one instance
- **Lazy Loading** — Servers start only when needed
- **Smart Routing** — Automatic language detection by file type
- **Full Coverage** — Hover, definitions, references, diagnostics, completions, rename, formatting, and more

## MCP Tools

| Tool | Description |
|------|-------------|
| `lsp_hover` | Documentation and type info |
| `lsp_definition` | Go to definition |
| `lsp_references` | Find all references |
| `lsp_diagnostics` | Errors and warnings |
| `lsp_rename` | Rename with dry-run preview |
| `lsp_completion` | Code completions |
| `lsp_formatting` | Format documents |
| ... | [See all 16 tools](https://github.com/Mark-Wells-Dev/Catenary/wiki/Overview#available-tools) |

## Documentation

- **[Install](https://github.com/Mark-Wells-Dev/Catenary/wiki/Install)** — Setup for Claude Code, Claude Desktop, Gemini CLI
- **[Config](https://github.com/Mark-Wells-Dev/Catenary/wiki/Config)** — Configuration reference
- **[LSPs](https://github.com/Mark-Wells-Dev/Catenary/wiki/LSPs)** — Language server setup guides

## License

**GPL-3.0** — See [LICENSE](LICENSE) for details.

**Commercial licensing** available for proprietary use. Contact `contact@markwells.dev`.

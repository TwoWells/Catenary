# Catenary

[![CI](https://github.com/MarkWells-Dev/Catenary/actions/workflows/ci.yml/badge.svg)](https://github.com/MarkWells-Dev/Catenary/actions/workflows/ci.yml)
[![CD](https://github.com/MarkWells-Dev/Catenary/actions/workflows/cd.yml/badge.svg)](https://github.com/MarkWells-Dev/Catenary/actions/workflows/cd.yml)

A bridge between [MCP](https://modelcontextprotocol.io/) and [LSP](https://microsoft.github.io/language-server-protocol/) — giving AI coding assistants IDE-quality code intelligence.

📚 **[Documentation](https://github.com/MarkWells-Dev/Catenary/wiki)**

## Quick Start

### 1. Install Catenary

```bash
cargo install catenary-mcp
```

### 2. Connect your AI Assistant

**Claude Code**
```bash
claude mcp add catenary -- catenary
```

**Gemini CLI**
Add to `~/.gemini/settings.json`:
```json
{
  "mcpServers": {
    "catenary": { "command": "catenary" }
  }
}
```

Then [configure your language servers](https://github.com/MarkWells-Dev/Catenary/wiki/Config) in `~/.config/catenary/config.toml`.

## Features

- **LSP Multiplexing** — Multiple language servers in one instance
- **Lazy Loading** — Servers start only when needed
- **Smart Routing** — Automatic language detection by file type
- **Full Coverage** — Hover, definitions, references, diagnostics, completions, rename, formatting, and more
- **File I/O** — Read, write, edit files with automatic LSP diagnostics
- **Shell Execution** — Run commands with configurable allowlists

## MCP Tools

| Tool | Description |
|------|-------------|
| `hover` | Documentation and type info |
| `definition` | Go to definition |
| `lsp_references` | Find all references |
| `diagnostics` | Errors and warnings |
| `rename` | Rename with dry-run preview |
| `completion` | Code completions |
| `formatting` | Format documents |
| `read_file` | Read file contents + diagnostics |
| `write_file` | Write file + diagnostics |
| `edit_file` | Search-and-replace edit + diagnostics |
| `list_directory` | List directory contents |
| `run` | Execute shell commands (allowlist enforced) |
| ... | [See all tools](https://github.com/MarkWells-Dev/Catenary/wiki/Overview#available-tools) |

## Documentation

- **[Install](https://github.com/MarkWells-Dev/Catenary/wiki/Install)** — Setup for Claude Code, Claude Desktop, Gemini CLI
- **[Config](https://github.com/MarkWells-Dev/Catenary/wiki/Config)** — Configuration reference
- **[LSPs](https://github.com/MarkWells-Dev/Catenary/wiki/LSPs)** — Language server setup guides

## License

**GPL-3.0** — See [LICENSE](LICENSE) for details.

**Commercial licensing** available for proprietary use. Contact `contact@markwells.dev`.

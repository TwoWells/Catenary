# Catenary Documentation

Welcome to the Catenary documentation — your guide to bringing IDE-quality code
intelligence to AI coding assistants.

## Quick Links

- **[Overview](overview.md)** — What Catenary is and what it does
- **[AI Agents](ai-agents.md)** — Guide for AI assistants using Catenary
- **[Installation](installation.md)** — Get Catenary running
- **[Configuration](configuration.md)** — Configure your language servers
- **[LSP Servers](lsp/README.md)** — Language server setup guides
- **[Roadmap](roadmap.md)** — What's next for Catenary

## What is Catenary?

Catenary bridges [MCP](https://modelcontextprotocol.io/) (Model Context
Protocol) and [LSP](https://microsoft.github.io/language-server-protocol/)
(Language Server Protocol), giving AI assistants like Claude access to real IDE
features: hover docs, go-to-definition, find references, diagnostics,
completions, rename, and more.

## Getting Started

**1. Install the binary**

```bash
cargo install catenary-mcp
```

**2. Configure language servers** — see [Configuration](configuration.md)

**3. Connect your AI assistant**

Plugins and extensions register the MCP server *and* hooks for post-edit
diagnostics, file locking, and root sync. The binary must be on your PATH.

*Claude Code:*
```
/plugin marketplace add https://github.com/MarkWells-Dev/Catenary
/plugin install catenary@catenary
```

*Gemini CLI:*
```bash
gemini extensions install https://github.com/MarkWells-Dev/Catenary
```

See [Installation](installation.md) for Claude Desktop, manual setup, and
other MCP clients.

**4. Set up language servers** — see [LSP Servers](lsp/README.md) for
per-language guides.

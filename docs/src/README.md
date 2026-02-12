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

```bash
# 1. Install Catenary
cargo install catenary-mcp

# 2. Create a config file
mkdir -p ~/.config/catenary
# See the Configuration page for details

# 3. Add to your MCP client (e.g., Claude Code)
claude mcp add catenary -- catenary
```

Then check the [LSP Servers](lsp/README.md) page to set up language servers for your stack.

# Catenary

A high-performance multiplexing bridge between MCP (Model Context Protocol) and LSP (Language Server Protocol). Enables LLMs to access IDE-grade code intelligence across multiple languages simultaneously with smart routing and UTF-8 accuracy.

## Installation

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

## Configuration

See `config.example.toml` in this directory or the [Official Configuration Guide](https://github.com/MarkWells-Dev/Catenary/wiki/Config).

## Documentation

For full features, tool lists, and troubleshooting, please visit the **[Main Repository](https://github.com/MarkWells-Dev/Catenary)**.

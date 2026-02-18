# Catenary

A high-performance multiplexing bridge between MCP (Model Context Protocol) and LSP (Language Server Protocol). Enables LLMs to access IDE-grade code intelligence across multiple languages simultaneously with smart routing and UTF-8 accuracy.

## Installation

### 1. Install Catenary

```bash
cargo install catenary-mcp
```

### 2. Install the Plugin

```
/plugin marketplace add https://github.com/MarkWells-Dev/Catenary
/plugin install catenary@catenary
```

The plugin configures the MCP server and adds a `PostToolUse` hook that returns LSP diagnostics after every edit.

## Configuration

See `config.example.toml` in this directory or the [Official Configuration Guide](https://markwells-dev.github.io/catenary/configuration.html).

## Documentation

For full features, tool lists, and troubleshooting, please visit the **[Main Repository](https://github.com/MarkWells-Dev/Catenary)**.

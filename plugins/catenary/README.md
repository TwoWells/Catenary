# Catenary

A high-performance multiplexing bridge between MCP (Model Context Protocol) and LSP (Language Server Protocol). Enables LLMs to access IDE-grade code intelligence across multiple languages simultaneously with smart routing and UTF-8 accuracy.

## Installation

### 1. Install the binary

```bash
cargo install catenary-mcp
```

The `catenary` binary must be on your PATH. The plugin does not include it —
it only registers hooks and the MCP server declaration. If the binary is
missing, hooks will silently do nothing.

### 2. Install the plugin

```
/plugin marketplace add https://github.com/MarkWells-Dev/Catenary
/plugin install catenary@catenary
```

The plugin registers the MCP server and adds hooks for post-edit diagnostics,
file locking, and workspace root sync.

## Configuration

See `config.example.toml` in this directory or the [Official Configuration Guide](https://markwells-dev.github.io/catenary/configuration.html).

## Documentation

For full features, tool lists, and troubleshooting, please visit the **[Main Repository](https://github.com/MarkWells-Dev/Catenary)**.

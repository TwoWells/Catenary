# Catenary

A high-performance multiplexing bridge between MCP (Model Context Protocol) and LSP (Language Server Protocol). Enables LLMs to access IDE-grade code intelligence across multiple languages simultaneously with smart routing and UTF-8 accuracy.

## Installation

```bash
# Install Catenary
cargo install catenary-mcp

# Add to Claude Code
claude mcp add catenary -- catenary

# Or install as a plugin
claude plugin marketplace add Mark-Wells-Dev/Catenary
claude plugin install catenary@catenary
```

## Configuration

See `config.example.toml` in this directory or the [Official Configuration Guide](https://github.com/Mark-Wells-Dev/Catenary/wiki/Config).

## Documentation

For full features, tool lists, and troubleshooting, please visit the **[Main Repository](https://github.com/Mark-Wells-Dev/Catenary)**.
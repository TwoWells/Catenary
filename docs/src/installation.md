# Install

## Prerequisites

- [Rust toolchain](https://rustup.rs/) (for installing via cargo)
- Language servers for the languages you want to use (see [LSP Servers](lsp/README.md))

## Install Catenary

### From crates.io (recommended)

```bash
cargo install catenary-mcp
```

### From source

```bash
git clone https://github.com/MarkWells-Dev/Catenary
cd Catenary
cargo build --release
# Binary is at ./target/release/catenary
```

## Add to Your MCP Client

### Claude Code (CLI)

**Option 1: Plugin (recommended)**

```bash
claude plugin marketplace add MarkWells-Dev/Catenary
claude plugin install catenary@catenary
```

**Option 2: Manual**

```bash
claude mcp add catenary -- catenary
```

### Claude Desktop

Add to your config file:
- **Linux:** `~/.config/claude/claude_desktop_config.json`
- **macOS:** `~/Library/Application Support/Claude/claude_desktop_config.json`

```json
{
  "mcpServers": {
    "catenary": {
      "command": "catenary"
    }
  }
}
```

### Gemini CLI

Add to `~/.gemini/settings.json`:

```json
{
  "mcpServers": {
    "catenary": {
      "command": "catenary"
    }
  }
}
```

### Other MCP Clients

```json
{
  "mcpServers": {
    "catenary": {
      "command": "catenary"
    }
  }
}
```

## Verify Installation

```bash
# Check catenary is in your PATH
which catenary

# Test it responds to MCP
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | catenary
```

## Next Steps

1. **[Configure](configuration.md)** your language servers
2. **[Install LSPs](lsp/README.md)** for your languages

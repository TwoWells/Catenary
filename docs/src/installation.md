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

> **The `catenary` binary must be installed and on your PATH before
> configuring any client.** Plugins and extensions provide hooks and MCP
> server declarations but do not include the binary. If the binary is
> missing, hooks will silently do nothing and you will get no diagnostics.

### Claude Code (CLI)

**Option 1: Plugin (recommended)**

```bash
claude plugin marketplace add MarkWells-Dev/Catenary
claude plugin install catenary@catenary
```

The plugin registers the MCP server and hooks for post-edit diagnostics,
file locking, and root sync. It requires the `catenary` binary on PATH.

**Option 2: Manual**

```bash
claude mcp add catenary -- catenary
```

This registers the MCP server only. You will not get post-edit diagnostics
or file locking unless you also configure hooks manually (see
[CLI Integration](cli-integration.md)).

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

**Option 1: Extension (recommended)**

```bash
gemini extensions install https://github.com/MarkWells-Dev/Catenary
```

The extension registers the MCP server and hooks for post-edit diagnostics
and file locking. It requires the `catenary` binary on PATH.

**Option 2: Manual**

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

This registers the MCP server only. You will not get post-edit diagnostics
or file locking unless you also install the extension or configure hooks
manually (see [CLI Integration](cli-integration.md)).

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

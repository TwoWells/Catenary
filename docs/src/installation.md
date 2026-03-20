# Install

## Prerequisites

- Language servers for the languages you want to use (see [LSP Servers](lsp/README.md))

## Install Catenary

### Download binary (recommended)

Download the latest release for your platform from
[GitHub Releases](https://github.com/MarkWells-Dev/Catenary/releases/latest):

| Platform     | Asset                       |
| :----------- | :-------------------------- |
| Linux x86_64 | `catenary-linux-amd64`      |
| Linux ARM64  | `catenary-linux-arm64`      |
| macOS x86_64 | `catenary-macos-amd64`      |
| macOS ARM64  | `catenary-macos-arm64`      |
| Windows      | `catenary-windows-amd64.exe`|

Place the binary somewhere on your `PATH`:

```bash
# Linux / macOS example
chmod +x catenary-linux-amd64
sudo mv catenary-linux-amd64 /usr/local/bin/catenary
```

### Quick install (Linux / macOS)

```bash
curl -fsSL https://raw.githubusercontent.com/MarkWells-Dev/Catenary/main/install.sh | sh
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

The plugin registers the MCP server and hooks for post-edit diagnostics
and root sync. It requires the `catenary` binary on PATH.

**Option 2: Manual**

```bash
claude mcp add catenary -- catenary
```

This registers the MCP server only. You will not get post-edit diagnostics
unless you also configure hooks manually (see
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

The extension registers the MCP server and hooks for post-edit diagnostics.
It requires the `catenary` binary on PATH.

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
unless you also install the extension or configure hooks manually (see
[CLI Integration](cli-integration.md)).

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

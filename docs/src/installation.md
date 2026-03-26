# Installation

## Prerequisites

- [Rust toolchain](https://rustup.rs/) (for building from source)
- Language servers for the languages you want to use (see [Language Servers](lsp/README.md))

## Install Catenary

```bash
cargo install --git https://github.com/MarkWells-Dev/Catenary catenary-mcp
```

## Connect to Your AI CLI

> The `catenary` binary must be on your PATH before configuring any client.
> Plugins and extensions provide hooks and MCP server declarations but do
> not include the binary.

### Claude Code (recommended: plugin)

```bash
claude plugin marketplace add MarkWells-Dev/Catenary
claude plugin install catenary@catenary
```

The plugin registers the MCP server and all hooks (post-edit diagnostics,
editing state enforcement, root sync, agent lifecycle).

### Gemini CLI (recommended: extension)

```bash
gemini extensions install https://github.com/MarkWells-Dev/Catenary
```

The extension registers the MCP server and all hooks.

### Manual MCP registration

For other clients, or if you prefer manual setup:

```json
{
  "mcpServers": {
    "catenary": {
      "command": "catenary"
    }
  }
}
```

This registers the MCP server only. Without the plugin/extension, you
will not get post-edit diagnostics or editing state enforcement.

## Verify

```bash
catenary doctor
```

For each configured server, `doctor` reports:

| Status | Meaning |
|--------|---------|
| `ready` | Server spawned, initialized, and capabilities listed |
| `command not found` | Binary not on `$PATH` |
| `spawn failed` | Binary found but process failed to start |
| `initialize failed` | Process started but LSP handshake failed |
| `skipped` | No files for this language in the workspace |

Use `--root` to check a different workspace:

```bash
catenary doctor --root /path/to/project
```

## Next Steps

1. [Configure](configuration.md) your language servers
2. [Install language servers](lsp/README.md) for your languages

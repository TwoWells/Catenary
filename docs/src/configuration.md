# Configuration

Catenary reads configuration from `~/.config/catenary/config.toml`.

## Basic Structure

```toml
# Global settings
idle_timeout = 300  # Seconds before closing idle documents (0 to disable)

# Language servers
[server.<language-id>]
command = "server-binary"
args = ["arg1", "arg2"]
```

## Example Config

```toml
idle_timeout = 300

[server.rust]
command = "rust-analyzer"

[server.python]
command = "pyright-langserver"
args = ["--stdio"]

[server.typescript]
command = "typescript-language-server"
args = ["--stdio"]

[server.javascript]
command = "typescript-language-server"
args = ["--stdio"]

[server.go]
command = "gopls"
```

## Language IDs

The `[server.<language-id>]` key must match the LSP language identifier. Common mappings:

| File Extension | Language ID |
|----------------|-------------|
| `.rs` | `rust` |
| `.py` | `python` |
| `.ts` | `typescript` |
| `.js` | `javascript` |
| `.go` | `go` |
| `.c` | `c` |
| `.cpp`, `.cc`, `.cxx` | `cpp` |
| `.sh`, `.bash` | `shellscript` |
| `.json` | `json` |
| `.yaml`, `.yml` | `yaml` |
| `.toml` | `toml` |
| `.md` | `markdown` |
| `.html` | `html` |
| `.css` | `css` |
| `.scss` | `scss` |
| `.php` | `php` |

See the [LSP Servers](lsp/README.md) page for language-specific configuration examples.

## Global Options

| Option | Default | Description |
|--------|---------|-------------|
| `idle_timeout` | `300` | Seconds before auto-closing idle documents. Set to `0` to disable. |

## CLI Override

You can also specify servers via CLI, which appends to (and overrides) the config file:

```bash
catenary --lsp "rust:rust-analyzer" --lsp "python:pyright-langserver --stdio"
```

This is useful for testing or one-off use without modifying your config.

## Tips

- **Lazy loading**: Servers only start when needed, so add as many as you like without overhead
- **One config**: All your MCP clients share the same config — change once, works everywhere
- **Args with spaces**: Use arrays for complex arguments: `args = ["--stdio", "--log-level", "debug"]`

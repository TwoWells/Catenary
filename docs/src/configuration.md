# Configuration

Catenary loads configuration from multiple sources, in order of priority (last one wins):

1.  **Defaults**: `idle_timeout = 300`.
2.  **User Config**: `~/.config/catenary/config.toml`.
3.  **Project Config**: `.catenary.toml` in the current directory or any parent directory (searches upwards).
4.  **Explicit File**: Specified via `--config <path>`.
5.  **Environment Variables**: Prefixed with `CATENARY_` (e.g., `CATENARY_IDLE_TIMEOUT=600`).
6.  **CLI Arguments**: `--lsp` and `--idle-timeout`.

## Basic Structure

```toml
# Global settings
idle_timeout = 300  # Seconds before closing idle documents (0 to disable)

# Language servers
[server.<language-id>]
command = "server-binary"
args = ["arg1", "arg2"]
```

## JSON Schema

A JSON schema is available in the repository at `catenary-config.schema.json`. You can use this to get autocompletion and validation in editors like VS Code.

To use it in VS Code, add this to your `settings.json`:

```json
"yaml.schemas": {
  "https://raw.githubusercontent.com/MarkWells-Dev/Catenary/main/catenary-config.schema.json": [".catenary.toml", "catenary.toml"]
}
```
*(Note: Requires the YAML extension which also handles TOML schemas in some versions, or use a dedicated TOML extension that supports `$schema` comments).*

## Example Config

```toml
idle_timeout = 300

[server.rust]
command = "rust-analyzer"

[server.rust.initialization_options]
check.command = "clippy"

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

[server.php]
command = "php-language-server"
```

## Initialization Options

Each server can receive custom `initialization_options` that are passed to the
LSP server during the `initialize` request. These are server-specific settings
that configure the server's behavior.

```toml
[server.rust]
command = "rust-analyzer"

[server.rust.initialization_options]
check.command = "clippy"
cargo.features = "all"
```

Refer to your language server's documentation for available options.

## Language IDs

The `[server.<language-id>]` key must match the LSP language identifier. Catenary detects these based on file extension and some common filenames:

| File / Extension | Language ID |
|------------------|-------------|
| `.rs` | `rust` |
| `.py` | `python` |
| `.ts` | `typescript` |
| `.tsx` | `typescriptreact` |
| `.js` | `javascript` |
| `.jsx` | `javascriptreact` |
| `.go` | `go` |
| `.c` | `c` |
| `.cpp`, `.cc`, `.cxx`, `.h`, `.hpp` | `cpp` |
| `.cs` | `csharp` |
| `.java` | `java` |
| `.kt`, `.kts` | `kotlin` |
| `.swift` | `swift` |
| `.rb` | `ruby` |
| `.php` | `php` |
| `.sh`, `.bash`, `.zsh` | `shellscript` |
| `Dockerfile` | `dockerfile` |
| `Makefile` | `makefile` |
| `CMakeLists.txt`, `.cmake` | `cmake` |
| `.json` | `json` |
| `.yaml`, `.yml` | `yaml` |
| `.toml`, `Cargo.toml`, `Cargo.lock` | `toml` |
| `.md` | `markdown` |
| `.html` | `html` |
| `.css` | `css` |
| `.scss` | `scss` |
| `.lua` | `lua` |
| `.sql` | `sql` |
| `.zig` | `zig` |
| `.mojo` | `mojo` |
| `.dart` | `dart` |
| `.m`, `.mm` | `objective-c` |
| `.nix` | `nix` |
| `.proto` | `proto` |
| `.graphql`, `.gql` | `graphql` |
| `.r`, `.R` | `r` |
| `.jl` | `julia` |
| `.scala`, `.sc` | `scala` |
| `.hs` | `haskell` |
| `.ex`, `.exs` | `elixir` |
| `.erl`, `.hrl` | `erlang` |

## Global Options

| Option | Default | Description |
|--------|---------|-------------|
| `idle_timeout` | `300` | Seconds before auto-closing idle documents. Set to `0` to disable. |

## CLI Override

You can also specify servers via CLI:

```bash
catenary --lsp "rust:rust-analyzer" --lsp "python:pyright-langserver --stdio"
```

## Verifying Your Setup

Use `catenary doctor` to check that configured language servers are working:

```bash
catenary doctor
```

For each configured server, `doctor` reports one of:

| Status | Meaning |
|--------|---------|
| `✓ ready` | Server spawned, initialized, and capabilities listed |
| `✗ command not found` | Binary not on `$PATH` |
| `✗ spawn failed` | Binary found but process failed to start |
| `✗ initialize failed` | Process started but LSP handshake failed |
| `- skipped` | No files for this language in the workspace |

Ready servers also list which Catenary tools they support (e.g. `hover`,
`definition`, `references`), based on the capabilities the server reports
during initialization.

Use `--nocolor` to disable colored output, or `--root` to check a different
workspace:

```bash
catenary doctor --root /path/to/project
```

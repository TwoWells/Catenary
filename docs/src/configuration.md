# Configuration

Catenary loads configuration from multiple sources, in order of priority (last one wins):

1.  **Defaults**: `idle_timeout = 300`, `log_retention_days = 7`.
2.  **User Config**: `~/.config/catenary/config.toml`.
3.  **Project Config**: `.catenary.toml` in the current directory or any parent directory (searches upwards).
4.  **Explicit File**: Specified via `--config <path>`.
5.  **Environment Variables**: Prefixed with `CATENARY_` (e.g., `CATENARY_IDLE_TIMEOUT=600`). Use `__` as a separator for nested keys (e.g., `CATENARY_ICONS__PRESET=nerd`).
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

[server.python.settings.python.analysis]
exclude = ["**/target", "**/node_modules"]

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

## Server Settings

Some language servers request configuration from the client via
`workspace/configuration`. Use the `settings` table to provide these values.
The TOML nesting mirrors the JSON object the server expects — Catenary matches
the `section` path from each configuration request item and returns the
corresponding subtree.

```toml
[server.python]
command = "pyright-langserver"
args = ["--stdio"]

[server.python.settings.python]
pythonPath = "/usr/bin/python3"

[server.python.settings.python.analysis]
exclude = ["**/target", "**/node_modules"]
extraPaths = []
```

When pyright sends `workspace/configuration` with
`{ "items": [{ "section": "python.analysis" }] }`, Catenary traverses
`settings["python"]["analysis"]` and returns
`{ "exclude": ["**/target", "**/node_modules"], "extraPaths": [] }`.

Items with no matching path receive `{}` (the default behavior).

Refer to your language server's documentation for available settings.

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
| `log_retention_days` | `7` | Days to keep dead session logs. `0` = remove all dead sessions on startup. `-1` = retain forever. |
| `tui.capture_tool_output` | `false` | Store full tool output in events for TUI detail expansion. Increases database size. |

## Icons

The `[icons]` table controls the icons shown in the [dashboard](dashboard.md)
event stream. Choose a base preset and optionally override individual icons.

### Presets

| Preset | Description |
|--------|-------------|
| `unicode` (default) | Safe symbols that render on any terminal font. |
| `nerd` | Nerd Font glyphs (requires a [patched font](https://www.nerdfonts.com/)). |

### Example

```toml
# Use Nerd Font icons
[icons]
preset = "nerd"
```

```toml
# Use Nerd Font icons but override lock/unlock
[icons]
preset = "nerd"
lock = "\U0001F512 "
unlock = "\U0001F511 "
```

### Override keys

Each key replaces the preset default for that icon slot. The value is an
arbitrary string (typically one or two characters plus a trailing space for
alignment).

| Key | Unicode default | Nerd default | Used for |
|-----|-----------------|--------------|----------|
| `diag_error` | `\u2717 ` (✗) | ` ` | Diagnostic error |
| `diag_warn` | `\u26A0 ` (⚠) | ` ` | Diagnostic warning |
| `diag_info` | `\u2139 ` (ℹ) | ` ` | Diagnostic info |
| `diag_ok` | `\u2713 ` (✓) | ` ` | Clean diagnostics |
| `lock` | `\u25B6 ` (▶) | ` ` | Lock acquired |
| `unlock` | `\u25C0 ` (◀) | ` ` | Lock released |
| `tool_search` | `\u2192 ` (→) | ` ` | Search tool |
| `tool_glob` | `\u2192 ` (→) | ` ` | Glob tool |
| `tool_default` | `\u2192 ` (→) | `\u2192 ` (→) | Fallback for other tools |

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

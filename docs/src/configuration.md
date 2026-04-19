# Configuration

Catenary loads configuration from multiple sources, in order of priority
(last wins):

1. **Defaults**: `log_retention_days = 7`.
2. **User config**: `~/.config/catenary/config.toml`.
3. **Project config**: `.catenary.toml` in the current directory or any parent (searches upward).
4. **Explicit file**: `--config <path>`.
5. **Environment variables**: Prefixed with `CATENARY_` (e.g., `CATENARY_LOG_RETENTION_DAYS=30`). Use `__` for nested keys (e.g., `CATENARY_ICONS__PRESET=nerd`).

## Language Servers

Configuration uses two sections: `[server.*]` defines how to run a
language server, and `[language.*]` binds languages to servers.

```toml
[server.<name>]
command = "server-binary"
args = ["arg1", "arg2"]

[language.<language-id>]
servers = ["<name>"]
```

### Example

```toml
[server.rust]
command = "rust-analyzer"

[server.rust.initialization_options]
check.command = "clippy"
cargo.features = "all"
diagnostics.disabled = ["inactive-code"]

[server.python]
command = "pyright-langserver"
args = ["--stdio"]

[server.python.settings.python]
pythonPath = "/usr/bin/python3"

[server.python.settings.python.analysis]
exclude = ["**/target", "**/node_modules"]
extraPaths = []

[server.tsserver]
command = "typescript-language-server"
args = ["--stdio"]

[server.gopls]
command = "gopls"

[language.rust]
servers = ["rust"]

[language.python]
servers = ["python"]

[language.typescript]
servers = ["tsserver"]

[language.go]
servers = ["gopls"]
```

### Initialization Options

Server-specific options passed during the LSP `initialize` request.
These go on the `[server.*]` entry:

```toml
[server.rust.initialization_options]
check.command = "clippy"
cargo.features = "all"
```

Refer to your language server's documentation for available options.

### Server Settings

Some language servers request configuration from the client via
`workspace/configuration`. The `settings` table provides these values
on the `[server.*]` entry. The TOML nesting mirrors the JSON object
the server expects — Catenary matches the `section` path from each
request and returns the corresponding subtree.

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
`{ "items": [{ "section": "python.analysis" }] }`, Catenary returns
`{ "exclude": ["**/target", ...], "extraPaths": [] }`.

Items with no matching path receive `{}`.

## Language IDs

The `[language.<language-id>]` key in the language section must match the LSP language identifier.
Catenary auto-detects languages from file extensions, filenames, and
shebangs (`#!` lines in extensionless scripts). Any language with an LSP
server works — this table covers what Catenary recognises automatically.

### By extension

| Extension | Language ID |
|-----------|-------------|
| `.rs` | `rust` |
| `.go` | `go` |
| `.c` | `c` |
| `.cpp`, `.cc`, `.cxx`, `.h`, `.hpp` | `cpp` |
| `.zig` | `zig` |
| `.d` | `d` |
| `.v` | `v` |
| `.nim` | `nim` |
| `.java` | `java` |
| `.kt`, `.kts` | `kotlin` |
| `.scala`, `.sc` | `scala` |
| `.groovy`, `.gvy` | `groovy` |
| `.clj`, `.cljs`, `.cljc` | `clojure` |
| `.cs` | `csharp` |
| `.fs`, `.fsx`, `.fsi` | `fsharp` |
| `.swift` | `swift` |
| `.m`, `.mm` | `objective-c` |
| `.py` | `python` |
| `.rb` | `ruby` |
| `.pl`, `.pm` | `perl` |
| `.php` | `php` |
| `.lua` | `lua` |
| `.tcl` | `tcl` |
| `.cr` | `crystal` |
| `.js`, `.mjs`, `.cjs` | `javascript` |
| `.ts`, `.mts`, `.cts` | `typescript` |
| `.tsx` | `typescriptreact` |
| `.jsx` | `javascriptreact` |
| `.hs`, `.lhs` | `haskell` |
| `.ml`, `.mli` | `ocaml` |
| `.elm` | `elm` |
| `.gleam` | `gleam` |
| `.ex`, `.exs` | `elixir` |
| `.erl`, `.hrl` | `erlang` |
| `.purs` | `purescript` |
| `.sh`, `.bash`, `.zsh`, `.ebuild`, `.eclass`, `.install` | `shellscript` |
| `.fish` | `fish` |
| `.ps1`, `.psm1`, `.psd1` | `powershell` |
| `.r`, `.R` | `r` |
| `.jl` | `julia` |
| `.mojo` | `mojo` |
| `.html`, `.htm` | `html` |
| `.css` | `css` |
| `.scss` | `scss` |
| `.sass` | `sass` |
| `.less` | `less` |
| `.svelte` | `svelte` |
| `.vue` | `vue` |
| `.json`, `.jsonc` | `json` |
| `.yaml`, `.yml` | `yaml` |
| `.toml` | `toml` |
| `.xml`, `.xsl`, `.xslt`, `.xsd` | `xml` |
| `.sql` | `sql` |
| `.graphql`, `.gql` | `graphql` |
| `.proto` | `proto` |
| `.md`, `.mdx` | `markdown` |
| `.rst` | `restructuredtext` |
| `.tex`, `.latex` | `latex` |
| `.typ` | `typst` |
| `.nix` | `nix` |
| `.tf`, `.tfvars` | `terraform` |
| `.cmake` | `cmake` |
| `.dart` | `dart` |
| `.dockerfile` | `dockerfile` |

### By filename

| Filename | Language ID |
|----------|-------------|
| `Dockerfile` | `dockerfile` |
| `Makefile`, `GNUmakefile` | `makefile` |
| `CMakeLists.txt` | `cmake` |
| `Cargo.toml`, `Cargo.lock` | `toml` |
| `Gemfile`, `Rakefile` | `ruby` |
| `Justfile`, `justfile` | `just` |
| `PKGBUILD` | `shellscript` |

### By shebang

For files without a recognised extension, Catenary reads the first line.
If it starts with `#!`, the interpreter name is matched:

| Interpreter | Language ID |
|-------------|-------------|
| `bash`, `sh`, `zsh`, `dash`, `ksh` | `shellscript` |
| `fish` | `fish` |
| `python`, `python3`, `python2` | `python` |
| `node`, `nodejs` | `javascript` |
| `deno` | `typescript` |
| `ruby`, `irb` | `ruby` |
| `perl` | `perl` |
| `php` | `php` |
| `lua`, `luajit` | `lua` |
| `tclsh`, `wish` | `tcl` |
| `Rscript` | `r` |
| `julia` | `julia` |
| `elixir`, `iex` | `elixir` |
| `erl` | `erlang` |
| `swift` | `swift` |
| `kotlin` | `kotlin` |
| `scala` | `scala` |
| `groovy` | `groovy` |
| `crystal` | `crystal` |

## Global Options

| Option | Default | Description |
|--------|---------|-------------|
| `log_retention_days` | `7` | Days to keep dead session data. `0` = remove on startup. `-1` = retain forever. |

## Notifications

The `[notifications]` table controls which tracing events are promoted
to user-facing notifications via the host CLI's `systemMessage`. See
[Notifications](notifications.md) for details on delivery timing, dedup,
and overflow.

```toml
[notifications]
threshold = "warn"    # default
```

| Option | Default | Description |
|--------|---------|-------------|
| `threshold` | `"warn"` | Minimum severity for notification delivery. One of `"debug"`, `"info"`, `"warn"`, `"error"`. |

## Icons

The `[icons]` table controls icons in the TUI dashboard.

| Preset | Description |
|--------|-------------|
| `unicode` (default) | Safe symbols for any terminal font. |
| `nerd` | Nerd Font glyphs (requires a [patched font](https://www.nerdfonts.com/)). |

```toml
[icons]
preset = "nerd"
```

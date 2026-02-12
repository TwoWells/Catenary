# Python

## Install

[Pyright](https://github.com/microsoft/pyright) is a fast, feature-rich Python language server from Microsoft.

### macOS

```bash
npm install -g pyright
```

Or via Homebrew:

```bash
brew install pyright
```

### Linux

```bash
npm install -g pyright
```

### Windows

```bash
npm install -g pyright
```

## Config

Add to `~/.config/catenary/config.toml`:

```toml
[server.python]
command = "pyright-langserver"
args = ["--stdio"]
```

## Notes

- Pyright provides type checking even for untyped code (infers types)
- Works well with virtual environments — activate your venv before starting your MCP client
- For Django/Flask projects, Pyright handles most patterns out of the box

## Alternatives

### Pylsp (python-lsp-server)

A community-maintained server with plugin support:

```bash
pip install python-lsp-server
```

```toml
[server.python]
command = "pylsp"
```

### Jedi Language Server

Lightweight, uses Jedi for completions:

```bash
pip install jedi-language-server
```

```toml
[server.python]
command = "jedi-language-server"
```

## Links

- [Pyright](https://github.com/microsoft/pyright)
- [python-lsp-server](https://github.com/python-lsp/python-lsp-server)

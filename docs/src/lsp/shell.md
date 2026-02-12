# Shell (Bash)

## Install

### macOS

```bash
npm install -g bash-language-server
```

### Linux

```bash
npm install -g bash-language-server
```

### Windows

```bash
npm install -g bash-language-server
```

## Config

Add to `~/.config/catenary/config.toml`:

```toml
[server.shellscript]
command = "bash-language-server"
args = ["start"]
```

## Notes

- The language ID is `shellscript`, not `bash` or `sh`
- Works with `.sh`, `.bash`, `.zsh` files
- Provides completions for commands, variables, and functions
- Integrates with [ShellCheck](https://www.shellcheck.net/) for linting (install separately)

## Optional: ShellCheck Integration

For better diagnostics, install ShellCheck:

```bash
# macOS
brew install shellcheck

# Linux (Debian/Ubuntu)
apt install shellcheck

# Linux (Arch)
pacman -S shellcheck
```

The language server will automatically use it if available.

## Links

- [bash-language-server](https://github.com/bash-lsp/bash-language-server)
- [ShellCheck](https://www.shellcheck.net/)

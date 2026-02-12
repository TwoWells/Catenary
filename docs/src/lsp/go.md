# Go

## Install

### macOS

```bash
go install golang.org/x/tools/gopls@latest
```

Or via Homebrew:

```bash
brew install gopls
```

### Linux

```bash
go install golang.org/x/tools/gopls@latest
```

### Windows

```bash
go install golang.org/x/tools/gopls@latest
```

## Config

Add to `~/.config/catenary/config.toml`:

```toml
[server.go]
command = "gopls"
```

## Notes

- `gopls` is the official Go language server
- Ensure `$GOPATH/bin` (or `$HOME/go/bin`) is in your PATH
- Works with Go modules out of the box
- First run indexes your module cache — may take a moment

## Links

- [gopls](https://pkg.go.dev/golang.org/x/tools/gopls)
- [gopls documentation](https://github.com/golang/tools/tree/master/gopls)

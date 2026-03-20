# TypeScript

## Install

### macOS

```bash
npm install -g typescript typescript-language-server
```

### Linux

```bash
npm install -g typescript typescript-language-server
```

### Windows

```bash
npm install -g typescript typescript-language-server
```

## Config

Add to `~/.config/catenary/config.toml`:

```toml
[server.typescript]
command = "typescript-language-server"
args = ["--stdio"]
```

## Notes

- The same server handles both TypeScript and JavaScript (see [JavaScript](javascript.md))
- Requires `typescript` as a peer dependency
- Works with `.ts`, `.tsx`, `.mts`, `.cts` files
- Reads your `tsconfig.json` for project settings

## Links

- [typescript-language-server](https://github.com/typescript-language-server/typescript-language-server)
- [TypeScript](https://www.typescriptlang.org/)

# JavaScript

JavaScript uses the same language server as TypeScript.

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
[language.javascript]
command = "typescript-language-server"
args = ["--stdio"]
```

## Notes

- Same server as [TypeScript](typescript.md) — install once, configure both
- Works with `.js`, `.jsx`, `.mjs`, `.cjs` files
- Provides type inference even in plain JavaScript
- Add a `jsconfig.json` to customize project settings

## JSX / React

JSX is handled automatically. Catenary ships a default `inherit` entry
that routes `javascriptreact` to the `javascript` server — no extra
config needed. To customize the variant independently:

```toml
[language.javascriptreact]
inherit = "javascript"
min_severity = "error"  # optional per-variant override
```

## Links

- [typescript-language-server](https://github.com/typescript-language-server/typescript-language-server)

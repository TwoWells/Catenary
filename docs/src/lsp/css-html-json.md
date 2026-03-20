# CSS, HTML, JSON

These three languages are bundled together in one package: `vscode-langservers-extracted`.

## Install

### macOS

```bash
npm install -g vscode-langservers-extracted
```

### Linux

```bash
npm install -g vscode-langservers-extracted
```

### Windows

```bash
npm install -g vscode-langservers-extracted
```

## Config

Add to `~/.config/catenary/config.toml`:

```toml
[server.css]
command = "vscode-css-language-server"
args = ["--stdio"]

[server.scss]
command = "vscode-css-language-server"
args = ["--stdio"]

[server.html]
command = "vscode-html-language-server"
args = ["--stdio"]

[server.json]
command = "vscode-json-language-server"
args = ["--stdio"]
```

## What's Included

The `vscode-langservers-extracted` package provides:

| Server | Languages |
|--------|-----------|
| `vscode-css-language-server` | CSS, SCSS, Less |
| `vscode-html-language-server` | HTML |
| `vscode-json-language-server` | JSON, JSONC |
| `vscode-markdown-language-server` | Markdown |
| `vscode-eslint-language-server` | ESLint |

## Notes

- These servers are extracted from VS Code, so they're well-maintained and feature-complete
- SCSS and Less use the same CSS server — it auto-detects the language
- For Tailwind CSS support, use `tailwindcss-language-server` (separate server)

## Links

- [vscode-langservers-extracted on npm](https://www.npmjs.com/package/vscode-langservers-extracted)

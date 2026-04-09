# PHP

## Install

[Intelephense](https://intelephense.com/) is the most popular PHP language server.

### macOS

```bash
npm install -g intelephense
```

### Linux

```bash
npm install -g intelephense
```

### Windows

```bash
npm install -g intelephense
```

## Config

Add to `~/.config/catenary/config.toml`:

```toml
[server.intelephense]
command = "intelephense"
args = ["--stdio"]

[language.php]
servers = ["intelephense"]
```

## Notes

- Intelephense has a free tier and a premium tier with additional features
- The free tier includes: completions, hover, definitions, references, diagnostics, formatting
- Premium adds: rename, code actions, go to implementation
- Works great with Laravel, Symfony, WordPress, and vanilla PHP

## Alternatives

### phpactor

A free, open-source alternative:

```bash
# Install via composer
composer global require phpactor/phpactor
```

```toml
[server.phpactor]
command = "phpactor"
args = ["language-server"]

[language.php]
servers = ["phpactor"]
```

## Links

- [Intelephense](https://intelephense.com/)
- [Intelephense on npm](https://www.npmjs.com/package/intelephense)
- [phpactor](https://phpactor.readthedocs.io/)

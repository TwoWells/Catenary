# Termux & Packaging

The `termux-language-server` provides advanced support for specialized shell scripts used in Termux, Arch Linux (PKGBUILD), Gentoo (ebuild), and Debian development.

## Install

### macOS

```bash
pip install termux-language-server
```

### Linux

```bash
pip install termux-language-server
```

### Windows

```bash
pip install termux-language-server
```

## Config

Add to `~/.config/catenary/config.toml`:

```toml
[server.termux-ls]
command = "termux-language-server"
args = ["--stdio"]

[language.termux]
servers = ["termux-ls"]

[language.pkgbuild]
servers = ["termux-ls"]

[language.ebuild]
servers = ["termux-ls"]

[language.eclass]
servers = ["termux-ls"]

[language.makepkg]
servers = ["termux-ls"]

[language.devscripts]
servers = ["termux-ls"]

[language.mdd]
servers = ["termux-ls"]

[language.subpackage]
servers = ["termux-ls"]

[language.install]
servers = ["termux-ls"]

[language.gentoo-make-conf]
servers = ["termux-ls"]

[language."make.conf"]
servers = ["termux-ls"]

[language."color.map"]
servers = ["termux-ls"]
```

## Notes

- This server is specifically designed for packaging and system-level shell scripts.
- It extends the features of `bash-language-server` for specialized formats.
- Supports file types like `PKGBUILD`, `build.sh`, `*.ebuild`, and `*.mdd`.

## Links

- [termux-language-server GitHub](https://github.com/termux/termux-language-server)

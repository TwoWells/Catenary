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
[language.termux]
command = "termux-language-server"
args = ["--stdio"]

[language.pkgbuild]
command = "termux-language-server"
args = ["--stdio"]

[language.ebuild]
command = "termux-language-server"
args = ["--stdio"]

[language.eclass]
command = "termux-language-server"
args = ["--stdio"]

[language.makepkg]
command = "termux-language-server"
args = ["--stdio"]

[language.devscripts]
command = "termux-language-server"
args = ["--stdio"]

[language.mdd]
command = "termux-language-server"
args = ["--stdio"]

[language.subpackage]
command = "termux-language-server"
args = ["--stdio"]

[language.install]
command = "termux-language-server"
args = ["--stdio"]

[language.gentoo-make-conf]
command = "termux-language-server"
args = ["--stdio"]

[language."make.conf"]
command = "termux-language-server"
args = ["--stdio"]

[language."color.map"]
command = "termux-language-server"
args = ["--stdio"]
```

## Notes

- This server is specifically designed for packaging and system-level shell scripts.
- It extends the features of `bash-language-server` for specialized formats.
- Supports file types like `PKGBUILD`, `build.sh`, `*.ebuild`, and `*.mdd`.

## Links

- [termux-language-server GitHub](https://github.com/termux/termux-language-server)

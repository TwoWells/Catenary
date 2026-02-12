# Rust

## Install

### macOS

```bash
rustup component add rust-analyzer
```

### Linux

```bash
rustup component add rust-analyzer
```

### Windows

```bash
rustup component add rust-analyzer
```

## Config

Add to `~/.config/catenary/config.toml`:

```toml
[server.rust]
command = "rust-analyzer"
```

## Notes

- rust-analyzer is the official Rust language server
- Installing via rustup ensures it stays in sync with your Rust toolchain
- First run on a project may take time to index (watch for "Indexing" status)

## Links

- [rust-analyzer](https://rust-analyzer.github.io/)
- [rust-analyzer User Manual](https://rust-analyzer.github.io/manual.html)

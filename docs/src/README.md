# Catenary

Catenary is an MCP server that gives AI coding agents LSP-powered code
intelligence. It multiplexes one or more language servers behind a single
MCP interface, providing search, diagnostics, and navigation without
shell-based text scanning.

Two MCP tools (`grep` and `glob`) plus post-edit diagnostics via hooks.
The agent never needs to know which language server handles which file.

- [Installation](installation.md) — install the binary and connect it to your CLI
- [Configuration](configuration.md) — configure language servers and settings
- [CLI & Dashboard](cli.md) — monitor sessions and browse events
- [Language Servers](lsp/README.md) — per-language setup guides

# Overview

## The Problem

AI coding agents navigate code by reading files and grepping for patterns.
This works, but it's wasteful.

Context windows are **append-only**. Every file the agent reads, every edit it
makes, every verification read — all of it accumulates. A single 500-line file
read-edited-verified three times puts three full copies into context. Every
token in that growing context is re-processed on every subsequent turn.

In practice, this creates a massive amplification effect. A few hours of work
can produce over **100 million tokens** of re-processed context, even though
the developer only typed a few thousand tokens of instructions. Most of that
is the model re-reading the same file contents over and over.

Bigger context windows don't fix this. They let you be wasteful for longer
before hitting the wall, but every token still costs compute and latency on
every turn. The problem scales with session length, not window size.

## The Solution

Catenary replaces brute-force file scanning with **graph navigation**.

Instead of grepping across 20 files to find a definition, `definition` returns
the exact location in one query. Instead of re-reading a file after editing it
to check for errors, the `catenary release` hook returns diagnostics inline.
Instead of fumbling with line numbers to find usages, `search` returns symbols,
semantic references, and text matches in a single call.

Each LSP query is small and stateless. Nothing accumulates. The context stays
lean across the entire session, regardless of how long the agent works.

| Brute force | Tokens | Context cost |
|-------------|--------|--------------|
| Read file to find type info | ~2,000 | +1 copy |
| Read file again after edit | ~2,000 | +1 copy (2 total) |
| Grep 20 files for a definition | ~8,000 | +20 partial copies |

| Graph navigation | Tokens | Context cost |
|------------------|--------|--------------|
| `search` for symbols + references | ~200 | stateless |
| Native edit + notify hook diagnostics | ~300 | no re-read |
| `definition` | ~50 | stateless |

## How It Works

```
┌─────────────┐     MCP      ┌──────────┐     LSP      ┌─────────────────┐
│ AI Assistant│◄────────────►│ Catenary │◄────────────►│ Language Server │
│ (Claude)    │              │          │              │ (rust-analyzer) │
└─────────────┘              │          │◄────────────►│ (pyright)       │
                             │          │              │ (gopls)         │
                             └──────────┘              └─────────────────┘
```

Catenary bridges [MCP](https://modelcontextprotocol.io/) and
[LSP](https://microsoft.github.io/language-server-protocol/). It manages
multiple language servers, routes requests by file type, and provides automatic
post-edit diagnostics via the `catenary release` hook — all through a single MCP
server. The agent never needs to know which server handles which language.

## Constrained Mode

Catenary is designed to be the agent's **primary navigation toolkit**, not a
supplement. In constrained mode, the host CLI's text-scanning commands (grep,
cat, find, ls, etc.) are denied via permissions, forcing the agent to use LSP
queries for navigation. The host's native file I/O tools remain available for
reading and editing, with Catenary providing post-edit diagnostics via the
`catenary release` hook.

See [CLI Integration](cli-integration.md) for setup instructions.

> Catenary also works as a supplement alongside built-in tools. But without
> constraints, agents default to what they were trained on — reading files and
> grepping — and the efficiency gains are lost.

## Features

| Feature               | Description                                                                            |
| --------------------- | -------------------------------------------------------------------------------------- |
| **LSP Multiplexing**  | Run multiple language servers in a single Catenary instance                            |
| **Eager Startup**     | Servers for detected languages start at launch; others start on first file access      |
| **Smart Routing**     | Requests automatically route to the correct server based on file type                  |
| **Universal Support** | Works with any LSP-compliant language server                                           |
| **Full LSP Coverage** | Search, diagnostics with quick fixes, workspace browsing, and more |
| **File I/O**          | Read, write, and edit files with automatic LSP diagnostics                            |

## Available Tools

### MCP Tools

| Tool       | Description                                         |
| ---------- | --------------------------------------------------- |
| `search`   | Search for symbols, semantic references, and text matches (LSP workspace symbols + references + file heatmap) |
| `glob`     | Browse the workspace — file outlines, directory listings, or glob pattern matches with symbols |

### Hook-Driven Features

| Feature          | Description                                         |
| ---------------- | --------------------------------------------------- |
| **Diagnostics**  | Errors, warnings, and quick-fix suggestions delivered inline after every edit via the `catenary release` PostToolUse hook |

File reading and editing is handled by the host tool's native file operations
(e.g. Claude Code's `Read`, `Edit`, `Write`). Catenary provides **post-edit
LSP diagnostics** (including quick-fix code actions) via the `catenary release`
hook — diagnostics and fix suggestions appear in the model's context after
every edit. See [CLI Integration](cli-integration.md) for hook configuration.

`search` (with the optional `paths` parameter) and `glob` can operate outside
workspace roots — access control is delegated to the host CLI's permission
layer. LSP-backed operations (diagnostics, hover, definition, etc.) are gated
to workspace roots because the language server has no knowledge of files
outside them. Catenary config files are always protected from modification.

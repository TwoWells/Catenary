# CLI & Dashboard

## Dashboard (TUI)

Running `catenary` in an interactive terminal launches the TUI dashboard.
When stdin and stdout are pipes (launched by an MCP client), it serves
MCP instead — no flags needed.

The dashboard is the primary way to observe Catenary. It shows all
sessions (active and historical), their language servers, and a live
stream of protocol messages (MCP, LSP, hooks). All messages are stored
in a SQLite database, so historical sessions can be browsed after the
fact.

```bash
catenary  # launch dashboard
```

### Keybindings

Keybinding hints appear in each pane's border.

**Sessions pane:**

| Key | Action |
|-----|--------|
| `j` / `Down` | Next session |
| `k` / `Up` | Previous session |
| `Space` | Toggle expand/collapse |
| `h` / `l` | Scroll horizontally (events) |
| `r` | Refresh |
| `x` | Delete session data (dead sessions only) |
| `q` / `Esc` | Quit |

**Events pane:**

| Key | Action |
|-----|--------|
| `j` / `Down` | Next event |
| `k` / `Up` | Previous event |
| `Space` | Toggle expand/collapse |
| `h` / `l` | Scroll horizontally |
| `Ctrl-u` | Page up |
| `Ctrl-d` | Page down |
| `G` | Jump to latest |
| `y` | Yank selected event |
| `f` | Open filter input |
| `F` | Clear filter |

## Protocol Transparency

Catenary logs every protocol message — every MCP tool call, every LSP
request and response, every hook invocation — to a local SQLite database.
The TUI shows the full message flow in real time: what Catenary sends to
your language servers, what they send back, and how long each exchange
takes.

You can see exactly what Catenary does. Nothing is hidden.

## CLI Commands

### `catenary list`

List active and historical sessions.

```bash
catenary list
```

### `catenary monitor <id>`

Stream events from a session to the terminal. Accepts a prefix of
either the Catenary session ID or the host CLI session ID.

```bash
catenary monitor 029b
catenary monitor 029b --raw       # raw JSON output
catenary monitor 029b --filter hover
```

### `catenary query`

Query events from the database. Useful for debugging and bug reports.

```bash
catenary query --session 029ba740 --since 1h
catenary query --kind diagnostics --since today
catenary query --search "hover" --format json
catenary query --sql "SELECT * FROM events WHERE payload LIKE '%timeout%'"
```

### `catenary gc`

Garbage-collect old session data.

```bash
catenary gc --older-than 7d
catenary gc --dead
catenary gc --session 029ba740
```

### `catenary doctor`

Verify language servers and hook installation. See [Installation](installation.md#verify).

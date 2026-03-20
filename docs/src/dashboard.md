# Dashboard

When you run `catenary` in an interactive terminal, it launches a TUI dashboard
for browsing active and historical sessions. When stdin and stdout are both
pipes (i.e. launched by an MCP client), Catenary serves MCP as usual — no flags
needed.

## Layout

The dashboard has two panes, each with keybinding hints in its bottom-right
border:

- **Session list** (top pane) — all known sessions sorted with active first,
  most recent at the top. Each row shows the session ID, workspace name, client
  name, and age. Active sessions display a green `●`; dead sessions show a dim
  `○`. If a session has running language servers, they appear on a second line.
- **Event tail** (bottom pane) — a live, colored stream of events from the
  selected session. Works for both active sessions (events arrive in real time)
  and dead sessions (historical replay). Events are color-coded: dim timestamps,
  cyan language tags, green/blue tool arrows, red errors, yellow warnings.
  Consecutive progress events for the same task are collapsed to the latest one.

## Keybindings

### Session list (top pane)

| Key | Action |
|-----|--------|
| `j` / `Down` | Select next session |
| `k` / `Up` | Select previous session |
| `r` | Refresh session list |
| `x` | Delete selected session's log (dead sessions only) |
| `q` / `Esc` | Quit |

### Event tail (bottom pane)

| Key | Action |
|-----|--------|
| `Ctrl-u` | Scroll events up (half page) |
| `Ctrl-d` | Scroll events down (half page) |
| `G` | Jump to latest event (clear scroll offset) |
| `f` | Open filter input |
| `F` | Clear active filter |

### Filter input mode

When filter input is active (after pressing `f`), the events pane border shows
a text input:

| Key | Action |
|-----|--------|
| *any character* | Append to filter string |
| `Backspace` | Delete last character |
| `Enter` | Apply filter |
| `Esc` | Cancel and return to normal mode |

The filter performs a case-insensitive substring match on the plain-text
rendering of each event line.

## Session Pruning

On startup, the dashboard automatically removes dead sessions older than
the configured retention period from the database. See
[`log_retention_days`](configuration.md#global-options) in the configuration
reference.

## Icons

Event icons in the dashboard are configurable via the `[icons]` table in your
config file. You can switch between a safe Unicode set (default) and Nerd Font
glyphs, or override individual icons. See
[`[icons]`](configuration.md#icons) in the configuration reference.

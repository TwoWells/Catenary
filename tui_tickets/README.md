# TUI Rewrite — Ticket Tracker

Tracks progress on the `catenary monitor` TUI rewrite described in `TUI_PLAN.md`.

## Module structure

The new TUI lives in `src/tui/` (replacing the old `src/tui.rs`):

```
src/tui/
  mod.rs          - pub fn run(), module declarations
  data.rs         - DataSource trait, LiveDataSource, MockDataSource
  theme.rs        - Theme, IconSet, event styling helpers
  app.rs          - App state, InputMode, FocusedPane
  layout.rs       - BSP layout engine + border junction characters
  tree.rs         - Sessions tree widget
  panel.rs        - Events panel: rendering, cursor, scroll, tail, expansion
  grid.rs         - Multi-panel grid, tab navigation, pinning
  scrollbar.rs    - Sub-character scrollbar + overflow counts
  selection.rs    - Visual selection mode + clipboard
  filter.rs       - Filter state, history, autocomplete UI
  mouse.rs        - Mouse event routing
  degradation.rs  - Responsive degradation chains
  hints.rs        - Navigation hints + cheatsheet
  render.rs       - Top-level draw function
```

## Build and test commands

**Do not run `cargo` directly.** All build, test, lint, and format commands
go through the Makefile. Direct `cargo build`, `cargo fmt`, `cargo clippy`,
etc. are denied by the constrained shell.

```
make test T=tui    # run TUI tests only
make check         # format + lint + deny + full test suite
```

All TUI tests live in `src/tui/*.rs` modules. The nextest filter `test(tui)`
matches any test with "tui" in its module path.

## Picking up a ticket

Find the first unchecked (`- [ ]`) ticket in the checklist below whose
dependencies are all checked (`- [x]`). Each ticket file lists its
dependencies at the top. Read the ticket file and execute it.

**Dependency quick-reference:** 00 has none. 01/02/03 need 00. 04 needs 03.
05 needs 01+03. 06/07 need 03. 08 needs 03+05. 09/10 need 01+02+03+05.
11 needs all others.

If multiple tickets are eligible, pick the lowest-numbered one.

## Ticket checklist

- [x] **00** — Module scaffold & data abstraction (`00_scaffold.md`)
- [x] **01** — BSP layout engine & border junctions (`01_layout.md`)
- [x] **02** — Sessions tree widget (`02_tree.md`)
- [x] **03** — Events panel core (`03_panel.md`)
- [ ] **04** — Event expansion & detail lines (`04_expansion.md`)
- [ ] **05** — Multi-panel grid & tab/pinning (`05_grid.md`)
- [ ] **06** — Sub-character scrollbar & overflow counts (`06_scrollbar.md`)
- [ ] **07** — Visual selection & copy (`07_selection.md`)
- [ ] **08** — Filter system (`08_filter.md`)
- [ ] **09** — Mouse support (`09_mouse.md`)
- [ ] **10** — Responsive degradation (`10_degradation.md`)
- [ ] **11** — Integration wiring, hints & cheatsheet (`11_integration.md`)

## Dependency graph

```
00 ─┬─► 01 ─┬─► 05 ─┬─► 08
    │        │        ├─► 09
    ├─► 02   │        ├─► 10
    │        │        │
    └─► 03 ──┤        │
         │   └────────┘
         ├─► 04
         ├─► 06
         └─► 07
                  all ──► 11
```

Tickets on the same level with no arrows between them can run in parallel
if separate agents are available.

## Completion protocol

After completing a ticket:

1. Run `make test T=tui` — all TUI tests must pass.
2. Run `make check` — full project build, lint, and test must pass.
3. Edit this file: change `- [ ]` to `- [x]` for your ticket.
4. Commit all changes:
   ```
   git add src/tui/ tui_tickets/README.md
   git commit -m "tui(XX): <short description of what was implemented>"
   ```
   Replace `XX` with your ticket number (e.g., `tui(00): module scaffold and data abstraction`).

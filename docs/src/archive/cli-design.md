# Archive: CLI Design

> **Status: Abandoned (2026-02-06)**
>
> This design was abandoned because subscription plans ($20/month Pro tier) are
> tied to official CLI tools (Claude Code, Gemini CLI). A custom CLI would
> require pay-per-token API access — wrong billing model for individual
> developers.
>
> See [CLI Integration](../cli-integration.md) for the current approach: disable
> built-in tools in existing CLIs, replace with catenary-mcp.

---

Original design document for `catenary-cli` — an AI coding assistant that owns
the model interaction loop.

## Problem

Existing AI coding tools (Claude Code, Gemini CLI) provide LSP tools but models
bypass them. They default to grep/read patterns from training data. Writes are
silent — no immediate feedback on errors.

The tools exist. Models don't use them.

**Root cause:** MCP tools are opt-in. The model chooses whether to use them.
Nothing enforces efficient patterns.

**Secondary issue:** These tools are built by companies that bill by usage.
Efficiency isn't incentivized.

## Solution

Catenary owns the outer loop. The model can't skip the feedback loop because
catenary-cli controls what tools exist and what results come back.

```
User → catenary-cli → Model API
                   ↓
            Tool execution (LSP-first)
                   ↓
            Feedback to model
```

## Design Principles

### Simple

One loop. No orchestrated modes. No sub-agents created and disposed
automatically. No "planning mode" that creates fresh contexts and forces
re-reading everything when it ends.

Planning happens in conversation — like any terminal session. The tool doesn't
impose structure.

### Fast

Execute immediately. Stream output. No artificial delays.

### Minimal

Expose tools. Let the model work. We control what tools exist and what feedback
comes back — not the model's reasoning process.

### Efficient

- LSP-first: hover instead of file read, symbols instead of grep
- Diagnostics on write: catch errors immediately, not 5 requests later
- Every token counts — users are on Pro tier ($20/month), not unlimited
- No throwaway contexts that need to be rebuilt

## Architecture

```
catenary-core/
├── LSP client management
├── Tool implementations
└── MCP type definitions (schema, not transport)

catenary-mcp/
└── MCP transport wrapper (JSON-RPC, stdio)

catenary-cli/
├── REPL loop
├── Model API client
└── Tool dispatch (calls core directly)
```

**MCP types as interface:** Core exposes tools using MCP type definitions. This
means:

- catenary-mcp wraps them for MCP transport
- catenary-cli uses them directly (no serialization overhead)
- Future tools just implement the MCP interface

**Open/closed:** Open to extension, closed to modification. Want a new tool?
Add it via MCP types. Core doesn't change.

## MVP Requirements

### REPL Loop

```
┌─────────────────────────────────────┐
│ catenary-cli (claude-sonnet-4-...) │
├─────────────────────────────────────┤
│ > user prompt                       │
│                                     │
│ [model streaming response...]       │
│                                     │
│ Tool: write_file                    │
│ Path: src/main.rs                   │
│ ┌─────────────────────────────────┐ │
│ │ - old line                      │ │
│ │ + new line                      │ │
│ └─────────────────────────────────┘ │
│ Allow? [y/n/e]:                     │
│                                     │
│ > _                                 │
└─────────────────────────────────────┘
```

**Core loop:**

1. Read user input
2. Send to model (stream response)
3. On tool call:
   - Display tool + args (diff for write/edit)
   - Await approval (single keypress)
   - Execute via catenary-core
   - Return result to model
   - Repeat if more tool calls
4. Display final response
5. Return to prompt

### Tool Approval

Every tool call requires explicit approval. No auto-approve mode.

- `y` — approve and execute
- `n` — reject, return rejection to model
- `e` — edit (for write/edit: open diff in $EDITOR)
- `?` — show explanation of what tool will do

**Why no auto-approve:** It's a trap. Models burn through tokens when
unchecked — reading 10 files when 1 would do, trying 5 command variants when
the first failed. The approval gate is a rate limiter and course-correction
point.

### Interrupt Handling

Ctrl+C cancels in-flight API request and returns to prompt cleanly.

### Minimum Tools

| Tool | Behavior |
|------|----------|
| `read_file` | Read file contents |
| `write_file` | Write + return diagnostic summary |
| `edit_file` | Edit + return diagnostic summary |
| `search` | LSP-backed, grep fallback (see below) |
| `build` | Run project build command |
| `test` | Run project tests |
| `git` | Status, diff, commit, push |
| `web_search` | Search the web |

**Write/edit feedback:** No silent writes. Every write returns diagnostic
summary (errors, warnings). The model can't proceed unaware that it broke
something.

### No Arbitrary Shell

No `shell` tool. Every action goes through a targeted MCP tool.

**Why:**

- Model can't bypass `search` with raw `grep`
- Model can't `cat` files instead of using `read_file`
- No accidental `rm -rf` or destructive commands
- Every action is intentional and auditable
- Token efficient — no parsing noisy shell output

**What shell typically does → MCP alternative:**

| Shell use case | MCP tool |
|----------------|----------|
| Build/compile | `build()` |
| Run tests | `test()` |
| Git operations | `git()` |
| Package install | `add_dependency()` |
| Run scripts | `run_script(path)` — curated list |
| File ops (mkdir, mv) | `mkdir()`, `move()`, `delete()` |
| Docker/k8s | User-configured MCP |
| Ansible | User-configured MCP |

**The long tail:** Users configure additional MCP tools for their workflow
(post-MVP scope). Model uses what's available, can't escape to raw shell.

The "limitation" is the feature. Intentionality over flexibility.

**Enforces good practices:**

Without shell, model can't run one-off validation scripts. It has to write
proper tests.

Old pattern (with shell):
1. Model writes code
2. Model runs `python test_quick.py` to validate
3. Model deletes `test_quick.py`
4. No trace, not repeatable

New pattern (no shell):
1. Model writes code
2. Model can only run `test()` — needs actual tests
3. Model writes proper test in test suite
4. Test is permanent, documented, repeatable

**Denial as teaching:**

```
Tool: delete("test_quick.py")
Allow? [y/n/e]: n

> Refactor this into a proper test

Model: "I'll add this to the test suite..."
```

User guides model toward better practices in real-time. The tool approval
isn't just safety — it's a feedback loop.

### Smart Search

`search(path, query)` — one tool, catenary handles routing.

**When LSP available:**

```
search("src/", "parse_config")
→ Results (via rust-analyzer):
  src/config.rs:42 — fn parse_config()  [definition]
```

Pinpoint accuracy. Definition vs usage distinguished.

**When LSP unavailable:**

```
search("src/", "parse_config")
→ Results (via grep — LSP unavailable):
  Note: grep cannot distinguish definition from usage.
  Results may include call sites. Definition may be in
  files outside search path.

  src/config.rs:42: fn parse_config()
  src/main.rs:15: parse_config()
  src/main.rs:89: parse_config()
  ...
```

Model sees the degradation, knows results are noisy. No silent fallback.

### LSP Monitoring

LSP session monitoring in MVP — essential for debugging when LSPs crash or
return unexpected results.

**Subcommands:**

```bash
catenary list      # show active LSP sessions
catenary monitor   # real-time event stream
```

**TUI integration:**

- `Ctrl+L` — toggle LSP monitor panel
- Status bar shows active LSP count/status
- See requests/responses in real-time

**Implementation:** Monitoring logic lives in catenary-core. Both CLI and MCP
binaries expose it. Core already has event broadcasting from Phase 4.5.

### LSP Recovery

User controls LSP failure recovery — no automatic retry loops.

**Crash during tool call:**

```
┌─────────────────────────────────────┐
│ ⚠ rust-analyzer crashed             │
│ [r]estart  [d]isable                │
└─────────────────────────────────────┘
```

- **Restart** — catenary restarts LSP, retries tool
- **Disable** — LSP disabled for session

**Background crash:**

- Status bar shows crash
- Non-blocking notification
- User addresses when ready

**Fallback mode (break glass):**

When model calls an LSP tool and LSP is unavailable:

1. **Skip user approval** — don't prompt for a broken tool
2. Return error immediately to model:
   ```
   LSP unavailable for rust. Use grep/glob for text search.
   Write/edit will work but diagnostics unavailable.
   ```
3. Model self-corrects and reaches for available tools

No silent tool swapping. No wasted user prompts. Model sees the limitation,
adapts its approach. Tool behavior stays consistent throughout session.

### Editor Integration

Full `$EDITOR` integration (neovim, vim, etc.) — no janky "vim mode" emulation.

**For prompt input:**

`Ctrl+G` opens `$EDITOR` with current input. User writes prompt with full editor
power, saves/quits, content returns to input box.

**For diff editing:**

`e` during tool approval opens `$EDITOR` with proposed changes. User edits,
saves/quits, edited content becomes the approved change.

**Implementation pattern:**

```
1. Write current content to temp file
2. Suspend TUI (LeaveAlternateScreen)
3. Spawn $EDITOR with temp file
4. Wait for editor to exit
5. Resume TUI (EnterAlternateScreen)
6. Read temp file, use as new content
```

Your editor, your config, your plugins.

### Display Requirements

- Show which model is active (in header/prompt)
- Show diff for write/edit before approval
- Stream model output as it arrives

## Future Scope (Post-MVP)

### Token/Request Monitoring

Real-time display of token usage and request count. Helps users stay within
tier limits.

### Additional MCP Tools

Allow configuration of external MCP servers for extended functionality.

### Context Management

When context window fills:

- Summarize conversation history
- Compact context
- Use local model (ollama/llama.cpp) for this — no API cost

**Model routing consideration:** Can't share tokens between Claude and Gemini.
Parallel contexts would double cost. If we add model routing, local models
handle the context bridge.

### Local Model Integration

Local models for supporting roles — not primary reasoning:

**Use cases:**

- **Embeddings** — semantic search over codebase
- **Context compression** — summarize history before API call
- **Context sanitization** — strip noise/secrets before sending to API

**Requirements:**

- **Transparent** — user sees when local compute is running, not hidden
- **Optional** — user can disable local compute entirely
- **Configurable** — works with 70B models (64GB RAM) or 300M models (8GB RAM)
- **Graceful degradation** — if no local model, skip the stage

```
User prompt
    ↓
[Local: sanitize/compress] ← optional, visible
    ↓
Claude API ← sees clean/small context
    ↓
Tool calls via catenary-core
    ↓
[Local: embed for search] ← optional, visible
```

Not everyone has 64GB unified memory. The tool works without local models but
benefits from them when available.

### Model Routing

Different models for different tasks:

- Claude: complex reasoning
- Gemini Flash: fast execution

Requires local model for context management. Not MVP scope.

## Implementation

### TUI Framework

**ratatui** — immediate-mode terminal UI framework.

- Widget-based: composable, reusable components
- Immediate-mode rendering: redraw from state each frame, no buffer accumulation
- Avoids the lag problem (Claude Code gets slow with long history)
- Already have `crossterm` in deps; ratatui uses it as backend

### Widgets (MVP)

| Widget | Purpose |
|--------|---------|
| Input | User prompt entry, Ctrl+G to $EDITOR |
| Conversation | Scrollable message history |
| Diff | Unified diff for write/edit approval |
| Tool approval | Tool name, args, y/n/e/? prompt |
| Status bar | Model name, connection status |

**Layout:**

```
┌─────────────────────────────────────┐
│ Status: claude-sonnet-4-...        │
├─────────────────────────────────────┤
│                                     │
│ [conversation / streaming output]   │
│                                     │
├─────────────────────────────────────┤
│ > user input                        │
└─────────────────────────────────────┘
```

**Tool approval replaces main area:**

```
┌─────────────────────────────────────┐
│ Tool: write_file                    │
│ Path: src/main.rs                   │
├─────────────────────────────────────┤
│ - fn old()                          │
│ + fn new()                          │
├─────────────────────────────────────┤
│ [y]es [n]o [e]dit [?]help           │
└─────────────────────────────────────┘
```

### Markdown Rendering

**tui-markdown** — converts markdown to ratatui `Text` type.

- Model outputs plain markdown
- `tui-markdown` parses and styles (headers, code blocks, bold, etc.)
- Includes `syntect` for code syntax highlighting
- Render result in `Paragraph` widget

### Alternate Screen Buffer

Use `crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen}`.

- Like vim/less — enter alternate buffer, exit cleanly
- Shell history untouched
- Suspend for $EDITOR, resume after

### Session Logging

```
~/.local/state/catenary/
├── sessions/
│   ├── 2026-02-06_103045.jsonl
│   └── 2026-02-06_142312.jsonl
└── current -> sessions/...
```

- XDG-compliant (`~/.local/state/`)
- JSONL format: one JSON object per message, easy to parse
- Full history in logs, viewport shows recent context

## Dependencies

**Required:**

- `ratatui` — TUI framework (MIT)
- `tui-markdown` — markdown to ratatui (MIT, includes syntect)
- `crossterm` — terminal backend (already in catenary)
- `reqwest` — HTTP client for model APIs
- `similar` or `diffy` — diff generation

**Future:**

- `ollama` client — local model management (MIT)
- Or `llama.cpp` bindings — raw inference (MIT)

## Open Decisions

Design questions to resolve before implementation.

### Model API

- [ ] Which model provider first? (Claude, Gemini, OpenAI)
- [ ] Use SDK crate or raw reqwest?
- [ ] Streaming response handling approach

### Authentication

- [ ] Where do API keys live? (env var, config file, keyring)
- [ ] Support multiple providers simultaneously?

### Configuration

- [ ] Config file location (`~/.config/catenary/cli.toml`?)
- [ ] What's user-configurable? (model, keybindings, theme)
- [ ] Runtime config changes or restart required?

### System Prompt

- [ ] Hardcoded base prompt?
- [ ] User-configurable additions?
- [ ] Per-session overrides?

### Context Management

- [ ] When to truncate conversation? (token limit)
- [ ] MVP: simple truncation or summarization?
- [ ] How to handle tool results in context?

### Diff Display

- [ ] Unified or side-by-side format?
- [ ] Which diff library? (`similar`, `diffy`)
- [ ] Syntax highlighting in diffs?

### Keybindings

- [ ] Fixed keybindings or customizable?
- [ ] Vim-style navigation in conversation?
- [ ] Document default keybindings

### Error Handling

- [ ] Network/API errors: inline, modal, or status bar?
- [ ] Tool execution errors: how to display?
- [ ] Retry logic for transient failures?

### Tool Interface

- [ ] How does catenary-core expose tools to CLI?
- [ ] Tool result format (structured or text?)
- [ ] Timeout handling for long-running tools

## Prototype

Validate the concept before building catenary-cli. Zero new code.

### Stack

```
mcphost (MIT)
├── disable built-in tools (omit from config)
├── catenary-mcp (already exists)
└── gemini-flash-lite (cheap, fast)
```

### Configuration

```json
{
  "mcpServers": {
    "catenary": {
      "command": "catenary-mcp"
    }
  }
}
```

No `fs`, no `bash`, no `http`. Model only has catenary tools.

### What We're Testing

- [ ] Model can only use catenary tools (no escape)
- [ ] Search uses LSP when available
- [ ] Search falls back to grep with degradation notice
- [ ] Write returns diagnostics
- [ ] Model adapts when LSP unavailable
- [ ] No shell bypass attempts

### Run It

```bash
mcphost --config catenary-only.json -- gemini-flash-lite
```

Give it a coding task. Watch behavior. Does it work? Does it try to escape?
Does it adapt?

### Success Criteria

If the model:
1. Uses catenary tools for file/search operations
2. Receives LSP-backed results (or graceful degradation)
3. Can't bypass to raw shell/grep
4. Completes coding tasks successfully

Then catenary-cli is just a polished TUI on top of this pattern.

### Why gemini-flash-lite

- Cheap (test iterations without cost concern)
- Fast (quick feedback loop)
- "Doer not thinker" — executes without overthinking
- If it works with flash-lite, it works with better models

## Non-Goals

- Pretty UI/animations
- Auto-approve mode
- Orchestrated modes (planning mode, proposal mode) that create/dispose contexts
- Automatic sub-agents that run in fresh contexts
- VSCode integration
- Mac-first design

This is a terminal tool for terminal users. Planning happens in conversation,
not in a special mode.

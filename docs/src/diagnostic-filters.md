# Diagnostic Filters

LSP servers attach boilerplate to diagnostic messages — reference URLs, lint
attribution lines, override instructions — that wastes tokens when delivered to
AI agents. Catenary's diagnostic filter system rewrites or drops these noisy
messages before they reach the model.

Filters are per-server, keyed by the server's command name (e.g.,
`"rust-analyzer"`). Servers without a filter get a default pass-through that
delivers messages unchanged.

## How It Works

The `DiagnosticFilter` trait in `src/filter/mod.rs` defines a single method:

```rust
pub trait DiagnosticFilter: Send + Sync {
    fn filter_message(
        &self,
        server: &str,
        version: Option<&str>,
        source: Option<&str>,
        code: Option<&DiagnosticCode>,
        severity: DiagnosticSeverity,
        language_id: &str,
        message: &str,
    ) -> String;
}
```

The return value controls what happens to the diagnostic:

| Return value | Effect |
|-------------|--------|
| Non-empty string | Deliver this message (original or rewritten) |
| Empty string | Drop the diagnostic entirely |

The `get_filter()` function in the same module dispatches to the correct
implementation based on the server command name.

## Existing Filters

### rust-analyzer

**File:** `src/filter/rust_analyzer.rs`

Strips the following noise patterns from clippy and rustc diagnostics:

| Pattern | Example |
|---------|---------|
| Clippy reference URLs | `for further information visit https://...` |
| Attribute lint attribution | `` `#[warn(unused_variables)]` on by default `` |
| Attribute implied-by | `` `#[warn(clippy::pedantic)]` implied by `#[warn(clippy::all)]` `` |
| Attribute override instructions | `` `#[allow(unused)]` to override `#[warn(unused_variables)]` `` |
| Flag lint attribution | `` `-W clippy::doc-markdown` implied by `-W clippy::pedantic` `` |
| Flag override instructions | `to override `-W clippy::pedantic` add `#[allow(clippy::doc_markdown)]`` |

**Version safety:** The filter only activates for rust-analyzer versions
starting with `1.`. Unrecognized major versions pass through unchanged — we
are writing regexes against output formats we don't own.

## Adding a New Filter

To add a filter for a new language server:

1. **Create the filter module.** Add a new file under `src/filter/` named after
   the server (e.g., `pyright.rs`). Implement `DiagnosticFilter` on a unit
   struct:

   ```rust
   use lsp_types::DiagnosticSeverity;
   use super::{DiagnosticCode, DiagnosticFilter};

   pub struct PyrightFilter;

   impl DiagnosticFilter for PyrightFilter {
       fn filter_message(
           &self,
           _server: &str,
           version: Option<&str>,
           _source: Option<&str>,
           _code: Option<&DiagnosticCode>,
           _severity: DiagnosticSeverity,
           _language_id: &str,
           message: &str,
       ) -> String {
           // Version guard — only filter known versions
           if !Self::is_known_version(version) {
               return message.to_string();
           }
           // Your filtering logic here
           message.to_string()
       }
   }
   ```

2. **Register the module.** In `src/filter/mod.rs`, add `mod pyright;` and
   extend `get_filter()` with a match arm for the server command:

   ```rust
   static PYRIGHT: pyright::PyrightFilter = pyright::PyrightFilter;

   match server_command {
       "rust-analyzer" => &RUST_ANALYZER,
       "pyright-langserver" => &PYRIGHT,
       _ => &DEFAULT,
   }
   ```

3. **Add tests.** Every filter pattern needs tests covering:
   - The noise pattern is stripped or dropped.
   - Clean messages pass through unchanged.
   - Unrecognized server versions pass through unchanged (version safety).

4. **Version guard.** Always gate your filter on a known version range. LSP
   servers can change their diagnostic message format between versions. If
   your regex matches something it shouldn't in a future version, the version
   guard ensures a safe pass-through.

### Guidelines

- **Default to pass-through.** If unsure whether a message is noise, keep it.
  A false positive (dropping a real diagnostic) is worse than a false negative
  (letting noise through).
- **Match the server command name**, not the language ID. Different servers for
  the same language (e.g., `pylsp` vs `pyright` vs `ruff`) produce different
  output and need separate filters.
- **Test against real output.** Capture actual diagnostic messages from the
  server and use them as test fixtures. Synthetic test strings may not match
  the real format.

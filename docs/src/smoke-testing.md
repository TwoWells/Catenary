# Smoke Testing

Manual verification procedures for features that depend on external state
(installed plugins, extension directories, PATH configuration) and cannot
be covered by unit or integration tests.

## `catenary doctor` — Hook Health Checks

The hooks section of `catenary doctor` compares installed hook files against
the hooks embedded in the binary at compile time. It also verifies PATH
consistency.

### Setup

Build and install the current binary:

```bash
cargo install --path .
```

### Claude Code Plugin

| Scenario | Steps | Expected |
|---|---|---|
| Plugin installed | Install via `/plugin install catenary@catenary` | Version, source type (directory/github), `✓ hooks match` |
| Plugin not installed | Remove via `/plugin remove catenary@catenary` | `- not installed` |
| Stale hooks | Edit `~/.claude/plugins/cache/catenary/catenary/<ver>/hooks/hooks.json` | `✗ stale hooks (reinstall: ...)` |
| Missing hooks file | Delete the cached `hooks/hooks.json` | `✗ hooks.json not found in plugin cache` |

### Gemini CLI Extension

| Scenario | Steps | Expected |
|---|---|---|
| Extension installed | `gemini extensions install https://github.com/MarkWells-Dev/Catenary` | Version, `(installed)`, `✓ hooks match` |
| Extension linked | `gemini extensions link /path/to/Catenary` | Version, `(linked)`, `✓ hooks match` |
| Extension not installed | `gemini extensions uninstall Catenary` | `- not installed` |
| Stale hooks (installed) | Edit `~/.gemini/extensions/Catenary/hooks/hooks.json` | `✗ stale hooks (update extension)` |

### PATH Consistency

| Scenario | Steps | Expected |
|---|---|---|
| PATH matches | `catenary doctor` from normal shell | `✓ /path/to/catenary` |
| PATH differs | Install a second copy elsewhere, prepend to PATH | `✗ /other/path differs from /original/path` |
| Not on PATH | Remove catenary from all PATH directories | `✗ catenary not found on PATH` |

### Version Header

`catenary doctor` prints the version from `git describe` at the top of
output. Verify it matches the expected format:

- Tagged commit: `Catenary 1.3.6`
- Post-tag: `Catenary 1.3.6-3-gabc1234`
- Dirty tree: `Catenary 1.3.6-3-gabc1234-dirty`

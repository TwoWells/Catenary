#!/usr/bin/env python3
"""PreToolUse/BeforeTool hook — allowlist-based Bash command filter.

Blocks text-scanning and file-peeking commands, steering agents toward
Catenary's LSP tools instead. Only explicitly allowed commands pass through.

Handles:
  - Full paths and env-var prefixes (e.g. DEBUG=1 make)
  - Sequential operators: &&, ||, ;
  - Pipelines: PIPELINE_SAFE commands (grep, head, tail, wc, jq) are allowed
    mid-pipeline (reading stdin) but denied at the start (reading files)
  - Subshell recursion: $(...), <(...), and `...` are recursively checked
  - Heredoc exception: cat/head/tail with << are allowed (reading stdin)

Usage
-----
Claude Code — symlink into ~/.claude/hooks/ then add to ~/.claude/settings.json:

    "hooks": {
      "PreToolUse": [
        {
          "matcher": "Bash",
          "hooks": [{"type": "command", "command": "$HOME/.claude/hooks/constrained-bash.py"}]
        }
      ]
    }

Gemini CLI — symlink into ~/.gemini/hooks/ then add to ~/.gemini/settings.json:

    "hooks": {
      "BeforeTool": [
        {
          "matcher": "run_shell_command",
          "hooks": [{"type": "command", "command": "$HOME/.gemini/hooks/constrained-bash.py --format=gemini"}]
        }
      ]
    }

See docs/src/cli-integration.md for full installation instructions.

Note: Claude Code snapshots hook scripts at session start. Changes to this
file take effect in new sessions only.
"""

import argparse
import json
import os
import re
import shlex
import sys

# Matches $(...), <(...), and `...` substitutions for recursive checking.
# [^)]* / [^`]* intentionally non-nested; deeply nested substitutions are
# not handled but are rare in practice.
_SUBSHELL_RE = re.compile(r'\$\(([^)]*)\)|<\(([^)]*)\)|`([^`]*)`')

# Matches heredoc start markers: <<EOF, <<'EOF', <<"EOF", <<-EOF, <<-'EOF', <<\EOF
_HEREDOC_MARKER_RE = re.compile(r'<<-?\s*\\?[\'"]?(\w+)[\'"]?')


def _strip_heredoc_bodies(cmd_string):
    """Remove heredoc bodies, keeping markers and closing delimiters.

    Heredoc bodies are literal text, not shell commands, but the recursive
    subshell checker would otherwise parse them as commands — triggering
    false denials on natural language containing ; && || or denied words.
    """
    lines = cmd_string.split('\n')
    result = []
    skip_until = None
    for line in lines:
        if skip_until is not None:
            if line.strip() == skip_until:
                skip_until = None
                result.append(line)
            continue
        result.append(line)
        m = _HEREDOC_MARKER_RE.search(line)
        if m:
            skip_until = m.group(1)
    return '\n'.join(result)


# Commands agents are allowed to run
ALLOWED = {"make", "git", "gh", "cp", "rm", "touch", "mkdir", "mv", "chmod", "sleep"}

# git subcommands that are denied even though git is allowed
DENIED_GIT = {"grep", "ls-files", "ls-tree"}

# Commands that are denied at the start of a pipeline (reading from files)
# but allowed mid-pipeline (reading from stdin).
PIPELINE_SAFE = {"grep", "egrep", "fgrep", "head", "tail", "wc", "jq"}

# Specific guidance for common denied commands
GUIDANCE = {
    "rg":      "Use Catenary's search tool instead.",
    "ag":      "Use Catenary's search tool instead.",
    "ack":     "Use Catenary's search tool instead.",
    "fd":      "Use Catenary's search tool instead.",
    "grep":    "Use Catenary's search tool instead.",
    "egrep":   "Use Catenary's search tool instead.",
    "fgrep":   "Use Catenary's search tool instead.",
    "rgrep":   "Use Catenary's search tool instead.",
    "zgrep":   "Use Catenary's search tool instead.",
    "ls":      "Use Catenary's list_directory tool instead.",
    "dir":     "Use Catenary's list_directory tool instead.",
    "tree":    "Use Catenary's list_directory tool instead.",
    "find":    "Use Catenary's list_directory tool instead.",
    "cat":     "Use the Read tool instead.",
    "head":    "Use the Read tool instead.",
    "tail":    "Use the Read tool instead.",
    "less":    "Use the Read tool instead.",
    "more":    "Use the Read tool instead.",
    "cargo":   "Use a make target instead. If no target exists, suggest one.",
    "rustc":   "Use a make target instead. If no target exists, suggest one.",
    "rustup":  "Use a make target instead. If no target exists, suggest one.",
    "prettier": "Use a make target instead. If no target exists, suggest one.",
}

DEFAULT_DENY = "Not available in constrained mode."


def find_command(tokens):
    """Skip leading variable assignments to find the actual command."""
    for i, token in enumerate(tokens):
        if re.match(r'^[A-Za-z_][A-Za-z_0-9]*=', token):
            continue
        return i
    return None


def check(cmd_string):
    """Check all commands in the string. Return deny reason or None."""
    cmd_string = _strip_heredoc_bodies(cmd_string)
    # Split on sequential operators first (&& || ;).
    # Do NOT split on bare | here — pipelines are handled in the inner loop so
    # we can allow PIPELINE_SAFE commands that read from stdin (mid-pipeline)
    # while still blocking them when they appear at the start (reading files).
    sequential = re.split(r"\s*(?:&&|\|\||;)\s*", cmd_string)
    for seq in sequential:
        # Split into pipeline stages on a bare | (not part of ||).
        stages = re.split(r"\s*(?<!\|)\|(?!\|)\s*", seq)
        for pipe_pos, segment in enumerate(stages):
            segment = segment.strip()
            if not segment:
                continue
            try:
                tokens = shlex.split(segment)
            except ValueError:
                tokens = segment.split()
            if not tokens:
                continue

            # Recursively check any $() or `` substitutions in the segment.
            # Applied to the raw segment string so the full $(cmd args) is
            # visible before shlex splits it into fragments.
            for m in _SUBSHELL_RE.finditer(segment):
                inner = (m.group(1) or m.group(2) or m.group(3) or "").strip()
                if inner:
                    reason = check(inner)
                    if reason:
                        return reason

            cmd_idx = find_command(tokens)
            if cmd_idx is None:
                continue
            name = os.path.basename(tokens[cmd_idx])
            rest = tokens[cmd_idx:]

            if name not in ALLOWED:
                # cat/head/tail with a heredoc read from stdin, not files — allow.
                if name in ("cat", "head", "tail") and any(t.startswith("<<") for t in rest):
                    continue
                # PIPELINE_SAFE commands mid-pipeline are reading from stdin — allow.
                if pipe_pos > 0 and name in PIPELINE_SAFE:
                    continue
                return GUIDANCE.get(name, DEFAULT_DENY)

            if name == "git" and len(rest) > 1 and rest[1] in DENIED_GIT:
                return "Use Catenary's search or list_directory tools instead."

    return None


def extract_command(data, fmt):
    """Extract the shell command string from hook JSON."""
    if fmt == "claude":
        if data.get("tool_name") != "Bash":
            return None
        return data.get("tool_input", {}).get("command", "")
    else:  # gemini
        if data.get("tool_name") != "run_shell_command":
            return None
        # Gemini may use tool_input or args depending on version
        tool_input = data.get("tool_input") or data.get("args") or {}
        return tool_input.get("command", "")


def deny_response(fmt, command, reason):
    """Build the host-specific denial JSON."""
    if fmt == "claude":
        return {
            "suppressOutput": True,
            "systemMessage": f"Blocked: {command}",
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            },
        }
    else:  # gemini
        return {"decision": "deny", "reason": reason}


def main():
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--format", choices=["claude", "gemini"], default="claude")
    args = parser.parse_args()

    try:
        data = json.load(sys.stdin)
    except (json.JSONDecodeError, EOFError):
        sys.exit(0)

    command = extract_command(data, args.format)
    if command is None:
        sys.exit(0)

    reason = check(command)
    if reason:
        json.dump(deny_response(args.format, command, reason), sys.stdout)


if __name__ == "__main__":
    main()

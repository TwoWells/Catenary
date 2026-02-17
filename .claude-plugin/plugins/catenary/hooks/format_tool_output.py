#!/usr/bin/env python3
"""Claude Code PreToolUse hook that formats Catenary file tool output.

- edit_file: shows a colorized unified diff instead of escaped strings
- write_file: shows a preview of the content being written
"""

import difflib
import json
import sys

RED = "\033[31m"
GREEN = "\033[32m"
CYAN = "\033[36m"
DIM = "\033[2m"
RESET = "\033[0m"

PREVIEW_LINES = 30


def format_edit(tool_input):
    file_path = tool_input.get("file", "unknown")
    old = tool_input.get("old_string", "")
    new = tool_input.get("new_string", "")

    if not old and not new:
        return None

    # Ensure trailing newlines so unified_diff produces clean output
    if old and not old.endswith("\n"):
        old += "\n"
    if new and not new.endswith("\n"):
        new += "\n"

    diff_lines = list(
        difflib.unified_diff(
            old.splitlines(keepends=True),
            new.splitlines(keepends=True),
            fromfile=file_path,
            tofile=file_path,
        )
    )

    if not diff_lines:
        return None

    colored = []
    for line in diff_lines:
        if line.startswith("---") or line.startswith("+++"):
            colored.append(f"{CYAN}{line}{RESET}")
        elif line.startswith("-"):
            colored.append(f"{RED}{line}{RESET}")
        elif line.startswith("+"):
            colored.append(f"{GREEN}{line}{RESET}")
        elif line.startswith("@@"):
            colored.append(f"{CYAN}{line}{RESET}")
        else:
            colored.append(line)

    return "".join(colored)


def format_write(tool_input):
    file_path = tool_input.get("file", "unknown")
    content = tool_input.get("content", "")

    if not content:
        return None

    lines = content.splitlines()
    total = len(lines)

    header = f"{CYAN}+++ {file_path}{RESET}\n"
    header += f"{DIM}{total} lines{RESET}\n"

    preview = lines[:PREVIEW_LINES]
    width = len(str(min(total, PREVIEW_LINES)))
    numbered = []
    for i, line in enumerate(preview, 1):
        numbered.append(f"{DIM}{i:>{width}}{RESET}  {GREEN}{line}{RESET}")

    body = "\n".join(numbered)

    if total > PREVIEW_LINES:
        body += f"\n{DIM}... {total - PREVIEW_LINES} more lines{RESET}"

    return f"{header}{body}\n"


def emit_result(formatted):
    """Emit the result JSON for Claude Code's PreToolUse hook."""
    json.dump(
        {
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecisionReason": f"\n{formatted}",
            }
        },
        sys.stdout,
    )


def main():
    data = json.load(sys.stdin)

    tool_name = data.get("tool_name", "")
    tool_input = data.get("tool_input", {})

    formatted = None

    if "edit_file" in tool_name:
        formatted = format_edit(tool_input)
    elif "write_file" in tool_name:
        formatted = format_write(tool_input)

    if formatted is None:
        return

    emit_result(formatted)


if __name__ == "__main__":
    main()

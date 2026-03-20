#!/usr/bin/env python3
"""Probe: sample page faults and CPU ticks for all catenary child processes.

Reads /proc/[pid]/stat for each LSP server process managed by catenary,
plus their children. Prints the work intensity metric:

    log(max(1, delta(pfc))) / delta(utime)

At configurable times, writes files to trigger LSP server activity:
  - t=5s:  append a type error to src/lib.rs
  - t=15s: restore src/lib.rs
  - t=25s: write a markdown file with a broken link
  - t=35s: fix the markdown file

Usage: python3 tools/settle_probe.py [--interval 0.1] [--duration 50]
"""

import argparse
import math
import os
import sys
import time


def find_catenary_pids():
    """Find all catenary process PIDs."""
    pids = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/comm") as f:
                comm = f.read().strip()
            if comm == "catenary":
                pids.append(int(entry))
        except (OSError, PermissionError):
            continue
    return pids


def get_children_ppid(parent_pid):
    """Find children by scanning /proc for matching PPIDs."""
    children = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat") as f:
                parts = f.read().split(")")[-1].split()
            ppid = int(parts[1])
            if ppid == parent_pid:
                children.append(int(entry))
        except (OSError, PermissionError, IndexError, ValueError):
            continue
    return children


def get_descendants(pid):
    """Get all descendant PIDs recursively via PPID scan."""
    descendants = []
    direct = get_children_ppid(pid)
    for child in direct:
        descendants.append(child)
        descendants.extend(get_descendants(child))
    return descendants


def read_stat(pid):
    """Read utime, stime, and page faults from /proc/[pid]/stat.

    Returns (utime, stime, pfc) or None if unreadable.
    """
    try:
        with open(f"/proc/{pid}/stat") as f:
            parts = f.read().split(")")[-1].split()
        minflt = int(parts[7])
        majflt = int(parts[9])
        utime = int(parts[11])
        stime = int(parts[12])
        return utime, stime, minflt + majflt
    except (OSError, PermissionError, IndexError, ValueError):
        return None


def get_cmdline(pid):
    """Get process command line, return short name."""
    try:
        with open(f"/proc/{pid}/cmdline") as f:
            raw = f.read()
        parts = raw.split("\0")
        for part in parts:
            if not part:
                continue
            base = os.path.basename(part)
            if base in ("node", "dotnet", "python3", "python", "bash", "sh"):
                continue
            return base
        return os.path.basename(parts[0]) if parts[0] else "?"
    except (OSError, PermissionError):
        return "?"


def sample_tree(catenary_pid):
    """Sample combined(utime) and combined(pfc) for each child tree."""
    direct_children = get_children_ppid(catenary_pid)

    result = {}
    for server_pid in direct_children:
        name = get_cmdline(server_pid)
        tree = [server_pid] + get_descendants(server_pid)

        combined_utime = 0
        combined_pfc = 0
        for pid in tree:
            stat = read_stat(pid)
            if stat:
                utime, stime, pfc = stat
                combined_utime += utime + stime
                combined_pfc += pfc

        result[server_pid] = (name, combined_utime, combined_pfc, len(tree))

    return result


# ── Stimuli ─────────────────────────────────────────────────────────

RUST_TYPE_ERROR = """
// settle_probe stimulus — will be removed
fn _settle_probe_bad() {
    let _x: i32 = "not a number";
}
"""

MD_BROKEN = """\
# Settle Probe

See [broken link](docs/src/boarddash.md) for details.
"""

MD_FIXED = """\
# Settle Probe

See [fixed link](docs/src/architecture.md) for details.
"""


def find_workspace_root(catenary_pid):
    """Try to find the workspace root from catenary's cwd."""
    try:
        return os.readlink(f"/proc/{catenary_pid}/cwd")
    except OSError:
        return os.getcwd()


class RustStimulus:
    """Append a type error to src/lib.rs, then restore."""

    def __init__(self, workspace):
        self.path = os.path.join(workspace, "src", "lib.rs")
        self.original = None

    def save(self):
        with open(self.path) as f:
            self.original = f.read()

    def inject_error(self):
        with open(self.path, "a") as f:
            f.write(RUST_TYPE_ERROR)

    def restore(self):
        if self.original is not None:
            with open(self.path, "w") as f:
                f.write(self.original)


class MarkdownStimulus:
    """Write/fix a markdown file with a broken link."""

    def __init__(self, workspace):
        self.path = os.path.join(workspace, "_settle_probe_test.md")

    def write_broken(self):
        with open(self.path, "w") as f:
            f.write(MD_BROKEN)

    def write_fixed(self):
        with open(self.path, "w") as f:
            f.write(MD_FIXED)

    def cleanup(self):
        try:
            os.unlink(self.path)
        except OSError:
            pass


def main():
    parser = argparse.ArgumentParser(description="Sample LSP server work intensity")
    parser.add_argument("--interval", type=float, default=0.1,
                        help="Sampling interval in seconds (default: 0.1)")
    parser.add_argument("--duration", type=float, default=50,
                        help="Total duration in seconds (default: 50)")
    parser.add_argument("--no-stimulus", action="store_true",
                        help="Disable file write stimuli (observe only)")
    args = parser.parse_args()

    catenary_pids = find_catenary_pids()
    if not catenary_pids:
        print("No catenary process found", file=sys.stderr)
        sys.exit(1)

    cat_pid = catenary_pids[0]
    workspace = find_workspace_root(cat_pid)

    # Initial discovery
    print(f"catenary PID: {cat_pid}")
    print(f"workspace: {workspace}")
    trees = sample_tree(cat_pid)
    print(f"\n  {len(trees)} server(s):")
    for server_pid, (name, c_utime, c_pfc, child_count) in trees.items():
        print(f"    {server_pid:>8}  {name:<30}  "
              f"utime={c_utime:>8}  pfc={c_pfc:>8}  children={child_count-1}")

    rust_stim = None
    md_stim = None
    stimuli = []

    if not args.no_stimulus:
        rust_stim = RustStimulus(workspace)
        md_stim = MarkdownStimulus(workspace)

        rust_stim.save()

        stimuli = [
            (5.0,  "INJECT rust type error → src/lib.rs", rust_stim.inject_error),
            (15.0, "RESTORE src/lib.rs",                  rust_stim.restore),
            (25.0, "WRITE markdown broken link",          md_stim.write_broken),
            (35.0, "WRITE markdown fixed link",           md_stim.write_fixed),
        ]

        print(f"\nStimuli scheduled:")
        for t, desc, _ in stimuli:
            print(f"    t={t:5.1f}s  {desc}")

    print(f"\nSampling every {args.interval}s for {args.duration}s\n")

    print(f"{'time':>8}  {'server':<30}  {'d(utime)':>8}  {'d(pfc)':>8}  "
          f"{'intensity':>10}  {'children':>8}")
    print("-" * 90)

    prev = {}
    start = time.time()
    stim_idx = 0

    # Seed prev
    for server_pid, (name, c_utime, c_pfc, child_count) in sample_tree(cat_pid).items():
        prev[server_pid] = (c_utime, c_pfc)

    try:
        while time.time() - start < args.duration:
            time.sleep(args.interval)
            elapsed = time.time() - start

            # Fire stimuli at scheduled times
            while stim_idx < len(stimuli) and elapsed >= stimuli[stim_idx][0]:
                t, desc, action = stimuli[stim_idx]
                action()
                print(f"{elapsed:8.2f}  >>> {desc}")
                stim_idx += 1

            trees = sample_tree(cat_pid)

            for server_pid, (name, c_utime, c_pfc, child_count) in trees.items():
                if server_pid in prev:
                    p_utime, p_pfc = prev[server_pid]
                    d_utime = c_utime - p_utime
                    d_pfc = c_pfc - p_pfc

                    if d_utime > 0:
                        intensity = math.log(max(1, d_pfc / d_utime))
                        print(f"{elapsed:8.2f}  {name:<30}  {d_utime:8d}  {d_pfc:8d}  "
                              f"{intensity:10.4f}  {child_count:8d}")
                    elif d_pfc > 0:
                        print(f"{elapsed:8.2f}  {name:<30}  {d_utime:8d}  {d_pfc:8d}  "
                              f"{'(no cpu)':>10}  {child_count:8d}")

                prev[server_pid] = (c_utime, c_pfc)

    finally:
        # Always restore
        if rust_stim:
            rust_stim.restore()
        if md_stim:
            md_stim.cleanup()

    print("\nDone.")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Dump GC blip samples from profiling databases.

GC blips: delta_pfc > 0 AND delta_utime = 0 AND delta_stime = 0.
Shows timing gaps between consecutive blips per server.
"""

import sqlite3
import sys
from pathlib import Path

DATA_DIR = Path(__file__).resolve().parent.parent / "data"

QUERY = """
SELECT timestamp_ms, server, pid, delta_pfc, delta_utime, delta_stime,
       in_progress, process_count
FROM intensity_samples
WHERE delta_pfc > 0 AND delta_utime = 0 AND delta_stime = 0
ORDER BY server, timestamp_ms
"""

ALL_SAMPLES_QUERY = """
SELECT timestamp_ms, server, pid, delta_pfc, delta_utime, delta_stime,
       in_progress, process_count
FROM intensity_samples
ORDER BY server, timestamp_ms
"""


def analyze_db(db_path: Path) -> None:
    print(f"\n{'=' * 70}")
    print(f"Database: {db_path.name}")
    print(f"{'=' * 70}")

    conn = sqlite3.connect(str(db_path))

    # GC blips
    rows = conn.execute(QUERY).fetchall()
    if not rows:
        print("  No GC blips found.")
        conn.close()
        return

    # Group by server
    by_server: dict[str, list] = {}
    for row in rows:
        ts, server, pid, pfc, ut, st, prog, pc = row
        by_server.setdefault(server, []).append(row)

    for server, blips in sorted(by_server.items()):
        print(f"\n  {server} — {len(blips)} GC blips")
        print(f"  {'ts_ms':>10}  {'pid':>7}  {'pfc':>5}  {'gap_ms':>8}")
        print(f"  {'-'*10}  {'-'*7}  {'-'*5}  {'-'*8}")

        prev_ts = None
        gaps = []
        for row in blips:
            ts, _, pid, pfc, _, _, _, _ = row
            gap = ""
            if prev_ts is not None:
                g = ts - prev_ts
                gaps.append(g)
                gap = str(g)
            print(f"  {ts:>10}  {pid:>7}  {pfc:>5}  {gap:>8}")
            prev_ts = ts

        if gaps:
            print(f"\n  Gaps: min={min(gaps)}ms  max={max(gaps)}ms  "
                  f"mean={sum(gaps)/len(gaps):.0f}ms  count={len(gaps)}")

    # Also show: right after computation stops, what do the first
    # few hundred ms look like?
    print(f"\n  --- Post-activity transition windows ---")

    all_rows = conn.execute(ALL_SAMPLES_QUERY).fetchall()
    by_server_all: dict[str, list] = {}
    for row in all_rows:
        by_server_all.setdefault(row[1], []).append(row)

    for server, samples in sorted(by_server_all.items()):
        # Find transitions: sample with cputime > 0 followed by cputime = 0
        transitions = []
        for i in range(1, len(samples)):
            prev = samples[i - 1]
            curr = samples[i]
            prev_cpu = prev[4] + prev[5]  # utime + stime
            curr_cpu = curr[4] + curr[5]
            if prev_cpu > 0 and curr_cpu == 0:
                # Found a transition. Grab next 20 samples.
                window = samples[i:i + 20]
                transitions.append((prev[0], window))

        if not transitions:
            continue

        print(f"\n  {server} — {len(transitions)} active→idle transitions")
        for t_idx, (trans_ts, window) in enumerate(transitions):
            print(f"\n    Transition at {trans_ts}ms:")
            print(f"    {'ts_ms':>10}  {'pid':>7}  {'pfc':>5}  {'ut':>4}  "
                  f"{'st':>4}  {'prog':>4}")
            print(f"    {'-'*10}  {'-'*7}  {'-'*5}  {'-'*4}  {'-'*4}  "
                  f"{'-'*4}")
            for s in window:
                ts, _, pid, pfc, ut, st, prog, _ = s
                marker = " ← GC" if pfc > 0 and ut == 0 and st == 0 else ""
                print(f"    {ts:>10}  {pid:>7}  {pfc:>5}  {ut:>4}  "
                      f"{st:>4}  {prog:>4}{marker}")

    conn.close()


def main() -> None:
    dbs = sorted(DATA_DIR.glob("intensity*.db"))
    if not dbs:
        print(f"No databases found in {DATA_DIR}", file=sys.stderr)
        sys.exit(1)

    for db in dbs:
        analyze_db(db)


if __name__ == "__main__":
    main()

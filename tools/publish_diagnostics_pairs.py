#!/usr/bin/env python3
"""Analyze publishDiagnostics from rust-analyzer for pairing patterns.

Queries the Catenary database for all publishDiagnostics notifications
from rust-analyzer in a given session. Groups by (uri, version) to see
if diagnostics arrive in pairs with the same version. Shows preceding
didOpen/didChange/didSave triggers to correlate pushes with triggers.

Usage: python3 tools/publish_diagnostics_pairs.py [client_session_id]
"""

import json
import os
import sqlite3
import sys
from collections import defaultdict
from pathlib import Path

SYNC_METHODS = {
    "textDocument/didOpen",
    "textDocument/didChange",
    "textDocument/didSave",
}


def db_path():
    override = os.environ.get("CATENARY_STATE_DIR")
    if override:
        return Path(override) / "catenary" / "catenary.db"
    xdg = os.environ.get("XDG_STATE_HOME")
    if xdg:
        return Path(xdg) / "catenary" / "catenary.db"
    return Path.home() / ".local" / "state" / "catenary" / "catenary.db"


def short_method(method):
    return method.rsplit("/", 1)[-1]


def extract_uri(payload):
    params = payload.get("params", payload)
    # didOpen has textDocument.uri, didChange/didSave have textDocument.uri
    td = params.get("textDocument", {})
    return td.get("uri", params.get("uri", "?"))


def shorten_uri(uri):
    return uri.split("/Catenary/")[-1] if "/Catenary/" in uri else uri


def main():
    mode = "grouped"  # default
    client_session_id = "e89a56e5"

    for arg in sys.argv[1:]:
        if arg == "--raw":
            mode = "raw"
        else:
            client_session_id = arg

    db = db_path()
    if not db.exists():
        print(f"Database not found: {db}", file=sys.stderr)
        sys.exit(1)

    conn = sqlite3.connect(str(db))
    conn.execute("PRAGMA journal_mode=WAL")

    # Find session(s) matching the client_session_id prefix
    rows = conn.execute(
        """SELECT id FROM sessions
           WHERE client_session_id LIKE ?
              OR id LIKE ?""",
        (f"%{client_session_id}%", f"%{client_session_id}%"),
    ).fetchall()

    if not rows:
        print(f"No sessions found for client_session_id prefix: {client_session_id}", file=sys.stderr)
        sys.exit(1)

    session_ids = [r[0] for r in rows]
    print(f"Sessions: {session_ids}\n")

    placeholders = ",".join("?" for _ in session_ids)

    # Fetch all sync notifications, publishDiagnostics, and progress, ordered by id
    all_methods = list(SYNC_METHODS) + [
        "textDocument/publishDiagnostics",
        "$/progress",
    ]
    method_placeholders = ",".join("?" for _ in all_methods)
    all_messages = conn.execute(
        f"""
        SELECT timestamp, method, payload
        FROM messages
        WHERE session_id IN ({placeholders})
          AND server = 'rust-analyzer'
          AND method IN ({method_placeholders})
        ORDER BY id ASC
        """,
        session_ids + all_methods,
    ).fetchall()

    # Fetch hook events (diagnostics) from the events table
    hook_events = conn.execute(
        f"""
        SELECT timestamp, kind, payload
        FROM events
        WHERE session_id IN ({placeholders})
          AND kind = 'diagnostics'
        ORDER BY id ASC
        """,
        session_ids,
    ).fetchall()

    # Merge hook events into all_messages as synthetic entries, sorted by timestamp
    for timestamp, _kind, payload_str in hook_events:
        all_messages.append((timestamp, "hook/post-tool", payload_str))
    all_messages.sort(key=lambda x: x[0])

    # Build per-URI timeline of triggers and pushes
    # Each entry: (timestamp, event_type, details)
    # event_type: "didOpen", "didChange", "didSave", "publish"
    by_uri = defaultdict(list)
    for timestamp, method, payload_str in all_messages:
        payload = json.loads(payload_str)

        if method == "$/progress":
            # Progress events are global, not per-URI — skip for grouped view
            continue

        uri = shorten_uri(extract_uri(payload))

        if method == "textDocument/publishDiagnostics":
            params = payload.get("params", payload)
            version = params.get("version")
            diags = params.get("diagnostics", [])
            sources = sorted({d.get("source", "?") for d in diags}) if diags else []
            by_uri[uri].append((timestamp, "publish", version, len(diags), sources))
        else:
            by_uri[uri].append((timestamp, short_method(method), None, None, None))

    if mode == "raw":
        print_raw_timeline(all_messages)
    else:
        print_grouped_timeline(by_uri)

    conn.close()


def print_raw_timeline(all_messages):
    """Chronological timeline of all events across all files."""
    print("=" * 90)
    print("RAW CHRONOLOGICAL TIMELINE")
    print("=" * 90)

    for timestamp, method, payload_str in all_messages:
        payload = json.loads(payload_str)

        if method == "hook/post-tool":
            file = payload.get("file", "?")
            count = payload.get("count", "?")
            preview = payload.get("preview", "").split("\n")[0][:60]
            short = shorten_uri(file)
            print(f"  {timestamp}  >>> hook done   diags={count}  {short}  {preview}")
            continue

        if method == "$/progress":
            params = payload.get("params", payload)
            token = params.get("token", "?")
            value = params.get("value", {})
            kind = value.get("kind")
            if kind in ("begin", "end"):
                title = value.get("title", "")
                label = f"progress {kind}"
                extra = f"  {title}" if title else ""
                print(f"  {timestamp}  {label:14s}  [{token}]{extra}")
            continue

        uri = shorten_uri(extract_uri(payload))

        if method == "textDocument/publishDiagnostics":
            params = payload.get("params", payload)
            version = params.get("version")
            diags = params.get("diagnostics", [])
            sources = sorted({d.get("source", "?") for d in diags}) if diags else []
            src_str = f"  [{', '.join(sources)}]" if sources else ""
            print(f"  {timestamp}  publish        v={version}  diags={len(diags)}{src_str}  {uri}")
        else:
            print(f"  {timestamp}  {short_method(method):14s}  {uri}")


def print_grouped_timeline(by_uri):
    """Per-file timeline with pair analysis."""
    print("=" * 80)
    print("PER-FILE TIMELINE (triggers + pushes)")
    print("=" * 80)

    pair_counts = defaultdict(int)

    for uri in sorted(by_uri):
        events = by_uri[uri]
        publishes = [e for e in events if e[1] == "publish"]
        if not publishes:
            continue

        print(f"\n{uri}  ({len(publishes)} pushes)")
        print("-" * 70)

        # Group publishes by version for pair counting
        pub_groups = []
        current_group = [publishes[0]]
        for pub in publishes[1:]:
            if pub[2] == current_group[0][2]:
                current_group.append(pub)
            else:
                pub_groups.append(current_group)
                current_group = [pub]
        pub_groups.append(current_group)
        for g in pub_groups:
            pair_counts[len(g)] += 1

        # Print full timeline
        for ts, event_type, version, n_diags, sources in events:
            if event_type == "publish":
                src_str = f"  [{', '.join(sources)}]" if sources else ""
                print(f"  {ts}  publish  v={version}  diags={n_diags}{src_str}")
            else:
                print(f"  {ts}  {event_type}")

    # Summary
    print("\n" + "=" * 80)
    print("SUMMARY: consecutive same-version group sizes")
    print("=" * 80)
    for size in sorted(pair_counts):
        label = "pairs" if size == 2 else "singles" if size == 1 else f"groups of {size}"
        print(f"  {size}: {pair_counts[size]}  ({label})")

    total_groups = sum(pair_counts.values())
    pairs = pair_counts.get(2, 0)
    if total_groups:
        print(f"\n  Total groups: {total_groups}")
        print(f"  Pairs: {pairs} ({100 * pairs / total_groups:.0f}%)")


if __name__ == "__main__":
    main()

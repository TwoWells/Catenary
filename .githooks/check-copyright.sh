#!/usr/bin/env bash
# Checks that every file passed as an argument starts with the SPDX header.
set -euo pipefail

EXPECTED_LINE1="// SPDX-License-Identifier: GPL-3.0-or-later"
EXPECTED_LINE2="// Copyright (C) 2026 Mark Wells <contact@markwells.dev>"

rc=0
for f in "$@"; do
    line1=$(sed -n '1p' "$f")
    line2=$(sed -n '2p' "$f")
    if [[ "$line1" != "$EXPECTED_LINE1" || "$line2" != "$EXPECTED_LINE2" ]]; then
        echo "Missing or incorrect SPDX header: $f"
        rc=1
    fi
done
exit $rc

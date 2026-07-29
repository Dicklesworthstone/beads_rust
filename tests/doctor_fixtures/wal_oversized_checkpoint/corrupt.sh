#!/usr/bin/env bash
# Fixture: wal_oversized_checkpoint
# FM: fm-state_files-wal-oversized (P2)
#
# Initialises a workspace, materialises a real SQLite WAL sidecar via
# `br create`, then appends ~33MB of zero padding to inflate it past
# the 32MB oversized threshold. The detector is purely size-based
# (fs::metadata stat — no DB open required) so the inflated file is
# enough to record `wal_size warn`.
#
# IMPORTANT — SQLite WAL lifecycle:
#   `br doctor` (Full mode) runs `check_sqlite_cli_integrity` which
#   spawns the `sqlite3` CLI against the LIVE DB. The CLI opens the
#   DB and on close considers our zero-padded WAL "fully consumed"
#   (the valid prefix is auto-checkpointed; the zero tail is past
#   EOF) and removes it. This happens AFTER `check_wal_oversized`
#   has already recorded `wal_size warn`, so the fixer still
#   activates on the (now-stale) initial report. The chokepoint
#   snapshot captures whatever WAL state remains at fixer-call
#   time, which on this codepath is "absent" — undo therefore
#   cannot restore the inflated WAL byte-for-byte. This is a
#   SQLite WAL-format limitation, not a chokepoint defect.
#
# To prevent the inner-gitignore fixer from running BEFORE the
# wal_checkpoint fixer (which would force a re-collect and drop
# the wal_size warn from the report), this fixture also writes
# the canonical inner-gitignore patterns the detector expects.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

# Satisfy the inner-gitignore detector so its fixer doesn't run
# before wal_checkpoint and force a re-collect that clears the
# wal_size warn. `br init` writes a template with `*.lock` and
# `*.tmp` globs, but the detector requires the literal
# `.write.lock` entry and `*.tmp` (literal). Append both
# canonical patterns; idempotent if already present.
for pattern in ".write.lock" "*.tmp"; do
    if ! grep -Fxq "$pattern" .beads/.gitignore 2>/dev/null; then
        printf '\n%s\n' "$pattern" >> .beads/.gitignore
    fi
done

# Force at least one DB write so SQLite materialises beads.db-wal
# with a valid 32-byte header. We need the header so the inflated
# file has a chance of being recognised as a "real" WAL by
# detectors that might evolve to validate WAL magic.
"$tool_bin" create "seed-wal-fixture" --type task --priority 2 \
    >/dev/null 2>&1 || true

# Some builds checkpoint-and-remove the WAL on connection close;
# if so, plant an empty file (the detector only stats the size).
if [ ! -f .beads/beads.db-wal ]; then
    : > .beads/beads.db-wal
fi

# Inflate WAL past the 32MB threshold (33MB ensures we comfortably
# cross). bs=1048576 count=33 appends exactly 33*1024*1024 bytes
# = 34,603,008; threshold is 32*1024*1024 = 33,554,432.
dd if=/dev/zero bs=1048576 count=33 status=none >> .beads/beads.db-wal

size_bytes=$(wc -c < .beads/beads.db-wal)
if [ "${size_bytes:-0}" -le 33554432 ]; then
    echo "corrupt.sh: WAL only $size_bytes bytes — expected > 32MB" >&2
    exit 1
fi

if [ -e .fixture_baseline ]; then
    echo "fixture baseline already exists; expected a fresh workspace" >&2
    exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

#!/usr/bin/env bash
# Fixture: orphan_shm_sidecar
# FM: fm-state_files-wal-shm-sidecar-orphan (P1) — variant: SHM without WAL
#
# Plants a .beads/beads.db-shm sidecar without a matching .beads/beads.db-wal.
# Triggers db.sidecars=error. `--repair` quarantines the orphan + rebuilds DB
# from JSONL. Round-trip is partial because the rebuild path predates the
# WP3/WP4 chokepoint migration — undo is a no-op for this fixture.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

# Remove the WAL sidecar that init created, then plant a bare SHM.
rm -f .beads/beads.db-wal
: > .beads/beads.db-shm

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

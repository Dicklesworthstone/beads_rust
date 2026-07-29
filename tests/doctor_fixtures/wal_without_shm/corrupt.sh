#!/usr/bin/env bash
# Fixture: wal_without_shm
# FM: fm-state_files-wal-shm-sidecar-orphan (P1) — WAL-without-SHM variant.
#
# This is the *expected* default state for frankensqlite (which uses WAL but
# eagerly removes SHM at session end), and the doctor reports it as
# severity=warn rather than error: "expected for frankensqlite". We exercise
# the BENIGN-warn detection path; `--repair` is a no-op for this variant
# because removing the WAL would discard committed-but-not-checkpointed data.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1
# Ensure SHM is absent (frankensqlite's default state after a clean exit).
rm -f .beads/beads.db-shm

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

#!/usr/bin/env bash
# Fixture: recovery_artifacts_orphaned
# FM: fm-state_files-recovery-artifacts-orphaned (P3)
#
# Plants stale .beads/.br_recovery/ artifacts from prior repair runs (rebuild-
# failed, .bad_, verification-failed). Detection via db.recovery_artifacts
# surfaces the count; current binary does not auto-prune (P3 cleanup gap).

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

mkdir -p .beads/.br_recovery
: > .beads/.br_recovery/beads.db.20250101_000000_aaaaa.bad_corruption
: > .beads/.br_recovery/beads.db.20250101_000001_bbbbb.bak
: > .beads/.br_recovery/rebuild-failed-20250102.json

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

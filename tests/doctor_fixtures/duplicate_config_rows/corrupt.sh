#!/usr/bin/env bash
# Fixture: duplicate_config_rows
# FM: fm-caches_indexes-* (recoverable anomalies — duplicate config key)
#
# Inserts two rows under the same `config.key`. Triggers
# `db.recoverable_anomalies` = error with "config contains duplicate rows
# for key" finding. `--repair` rebuilds via JSONL, collapsing duplicates.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1
"$tool_bin" create --title "dup-cfg seed" --type task --priority 2 --json >/dev/null
"$tool_bin" sync --flush-only >/dev/null 2>&1 || true

# Plant duplicate config rows under a synthetic key.
printf '%s\n' \
  "INSERT INTO config(key, value) VALUES('fixture.dup_test', 'first');" \
  "INSERT INTO config(key, value) VALUES('fixture.dup_test', 'second');" \
  | sqlite3 .beads/beads.db

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

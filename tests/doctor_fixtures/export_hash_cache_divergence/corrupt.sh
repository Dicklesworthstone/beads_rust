#!/usr/bin/env bash
# Fixture: export_hash_cache_divergence
# FM: fm-caches_indexes-export-hash-cache-divergence (P1) — top-level hash variant
#
# Creates a workspace with a populated metadata.jsonl_content_hash row,
# then overwrites that row with a known-wrong value while leaving the
# JSONL on disk untouched. Doctor's `db.export_hash_cache` check should
# fire warn; `--repair` should recompute and restore.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

# Create a couple of issues + sync to populate metadata.jsonl_content_hash
# and the export_hashes table.
"$tool_bin" create --title "fixture issue 1" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" create --title "fixture issue 2" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

# Capture the JSONL bytes pre-corruption so we can later assert they're
# unchanged by --repair.
sha256sum .beads/issues.jsonl | awk '{print $1}' > .fixture_jsonl_pre_sha256

# Poison the cached top-level hash with a known-wrong value. JSONL on
# disk is untouched. Uses python3 because the sqlite3 CLI isn't
# guaranteed in the test harness env.
python3 <<'PY'
import sqlite3
conn = sqlite3.connect(".beads/beads.db")
cur = conn.cursor()
cur.execute("UPDATE metadata SET value = 'deadbeef-poisoned' WHERE key = 'jsonl_content_hash'")
conn.commit()
conn.close()
PY

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

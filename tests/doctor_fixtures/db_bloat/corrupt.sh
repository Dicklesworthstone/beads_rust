#!/usr/bin/env bash
# Fixture: db_bloat
# FM: fm-caches_indexes-db-bloat-vs-jsonl
#
# Builds a valid workspace whose JSONL is large enough to make the ratio
# meaningful, then appends zero pages to the SQLite database. SQLite accepts
# the file and VACUUM rewrites it back to the logical database size, which
# makes this a deterministic end-to-end check for the unsafe-auto-fix path
# without issuing destructive SQL in the fixture.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

# The detector intentionally ignores tiny workspaces. Keep this comfortably
# above DB_BLOAT_MIN_JSONL_BYTES (1 MiB) with a single valid issue record.
payload_file=".fixture_large_description"
head -c 1200000 /dev/zero | tr '\0' 'x' > "$payload_file"
{
  printf '{"id":"bd-bloat-001","title":"db bloat fixture","status":"open","priority":2,"issue_type":"task","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","assignee":null,"labels":[],"description":"'
  cat "$payload_file"
  printf '","acceptance_criteria":"","dependencies":[],"epic_id":null,"discovered_by":null,"discovered_from":null,"source_repo":null,"design":null,"notes":null,"closed_at":null,"close_reason":null}\n'
} > .beads/issues.jsonl

"$tool_bin" sync --import-only --rebuild >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

# python3's sqlite3 module instead of the sqlite3 CLI: CI runners and the RCH
# workers all have the former, not always the latter.
integrity_ok() {
  python3 - "$1" "$2" <<'PY'
import sqlite3
import sys

conn = sqlite3.connect(sys.argv[1])
if sys.argv[2] == "checkpoint":
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
row = conn.execute("PRAGMA integrity_check").fetchone()
conn.close()
sys.exit(0 if row and row[0] == "ok" else 1)
PY
}

integrity_ok .beads/beads.db checkpoint

# Add 18 MiB of trailing zero pages. SQLite still reports integrity_check=ok,
# and VACUUM compacts the database back to its logical page set.
dd if=/dev/zero bs=1048576 count=18 status=none >> .beads/beads.db
integrity_ok .beads/beads.db no-checkpoint

wc -c < .beads/beads.db > .fixture_db_bloat_pre_bytes
wc -c < .beads/issues.jsonl > .fixture_jsonl_bytes
sha256sum .beads/beads.db | awk '{print $1}' > .fixture_db_bloat_pre_sha256

printf 'BR_DOCTOR_FIXTURE_REPAIR_ARGS=--unsafe-auto-fix --only fm-caches_indexes-db-bloat-vs-jsonl\n' \
  > .fixture_env

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .


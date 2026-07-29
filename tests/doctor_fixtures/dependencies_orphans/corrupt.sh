#!/usr/bin/env bash
# Fixture: dependencies_orphans
# FM: fm-caches_indexes-dependencies-orphans (P2) — orphan rows in
# the dependencies table on either the issue_id side or the local
# depends_on_id side.
#
# Inserts four dependency rows after init+sync, with FK guard off:
#   - row A: orphan issue_id ('bd-orphan-owner' not in issues)
#   - row B: orphan local depends_on_id ('bd-missing-target' not in
#     issues, not external)
#   - row C: external cross-repo ref ('external:upstream-1') — MUST
#     survive --repair
#   - row D: valid local link (both ends in issues) — MUST survive
# Doctor should detect orphan_count=2 and prune A and B while
# preserving C and D.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

"$tool_bin" create --title "valid owner A" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" create --title "valid target A" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" create --title "external-ref owner" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

# Capture the issue IDs we just created (they're hash-based so we
# read them back from JSONL). We need at least two real IDs so the
# valid + external rows attach to live issues.
ids=$(python3 <<'PY'
import json
ids = []
with open(".beads/issues.jsonl", "r", encoding="utf-8") as f:
    for line in f:
        line = line.strip()
        if not line or line.startswith("{\"merge_state"):
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(rec, dict) and "id" in rec:
            ids.append(rec["id"])
print("\n".join(ids))
PY
)
mapfile -t id_array <<< "$ids"
if [ "${#id_array[@]}" -lt 3 ]; then
  echo "corrupt: expected >=3 issues in JSONL, got ${#id_array[@]}" >&2
  exit 1
fi
valid_owner="${id_array[0]}"
valid_target="${id_array[1]}"
external_owner="${id_array[2]}"

sha256sum .beads/issues.jsonl | awk '{print $1}' > .fixture_jsonl_pre_sha256
{
  echo "$valid_owner"
  echo "$valid_target"
  echo "$external_owner"
} > .fixture_live_ids

python3 - "$valid_owner" "$valid_target" "$external_owner" <<'PY'
import sqlite3, sys
valid_owner, valid_target, external_owner = sys.argv[1], sys.argv[2], sys.argv[3]
conn = sqlite3.connect(".beads/beads.db")
cur = conn.cursor()
cur.execute("PRAGMA foreign_keys = OFF")
rows = [
    ("bd-orphan-owner", "bd-other", "blocks"),           # A: issue_id orphan
    (valid_owner, "bd-missing-target", "blocks"),         # B: local depends_on_id orphan
    (external_owner, "external:upstream-1", "blocks"),    # C: external — preserve
    (valid_owner, valid_target, "blocks"),                # D: valid link — preserve
]
for issue_id, depends_on_id, kind in rows:
    cur.execute(
        "INSERT INTO dependencies(issue_id, depends_on_id, type) VALUES (?, ?, ?)",
        (issue_id, depends_on_id, kind),
    )
conn.commit()
conn.close()
PY

# Snapshot the orphan keys so post_undo can verify identity restore.
{
  echo "bd-orphan-owner|bd-other"
  echo "${valid_owner}|bd-missing-target"
} > .fixture_orphan_keys
# Snapshot the survivor keys so post_repair can verify they remained.
{
  echo "${external_owner}|external:upstream-1"
  echo "${valid_owner}|${valid_target}"
} > .fixture_survivor_keys

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

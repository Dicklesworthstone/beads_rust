#!/usr/bin/env bash
# Fixture: dep_dead_closed_blocking_edges
# FM: fm-dependencies-dead-closed-blocking-edges (P3) +
#     fm-dependencies-fully-unblocked-open-issues (P3) — issue #350
#     dependency-graph JSONL audit.
#
# Plants the canonical stale-edge shape with nothing but public CLI
# commands: an open issue whose only `blocks` dependency targets a
# blocker that has since been closed. The edge is dead (its blocker is
# terminal) AND the open issue is fully unblocked (every declared
# blocker is dead), so one planted state fires both detect-only checks:
#   - dep.dead_closed_blocking_edges  (warn)
#   - dep.fully_unblocked_open        (warn)

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

"$tool_bin" create --title "closed blocker" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" create --title "open blocked issue" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

# Hash-based IDs: read them back from the JSONL by title.
ids=$(python3 <<'PY'
import json
by_title = {}
with open(".beads/issues.jsonl", "r", encoding="utf-8") as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(rec, dict) and "id" in rec:
            by_title[rec.get("title", "")] = rec["id"]
print(by_title.get("closed blocker", ""))
print(by_title.get("open blocked issue", ""))
PY
)
mapfile -t id_array <<< "$ids"
blocker_id="${id_array[0]}"
blocked_id="${id_array[1]}"
if [ -z "$blocker_id" ] || [ -z "$blocked_id" ]; then
  echo "corrupt: could not resolve fixture issue ids from JSONL" >&2
  exit 1
fi

# blocked depends on blocker (forward `blocks` edge), then the blocker
# closes WITHOUT the edge being removed — the exact hygiene gap the
# #350 audit exists to surface.
"$tool_bin" dep add "$blocked_id" "$blocker_id" >/dev/null 2>&1
"$tool_bin" close "$blocker_id" --reason "fixture: blocker completed" >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

{
  echo "$blocker_id"
  echo "$blocked_id"
} > .fixture_ids
sha256sum .beads/issues.jsonl | awk '{print $1}' > .fixture_jsonl_pre_sha256

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

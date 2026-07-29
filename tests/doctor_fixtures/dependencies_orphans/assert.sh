#!/usr/bin/env bash
# Fixture assertions: dependencies_orphans
#
# Pass-5 cycle 36: fm-caches_indexes-dependencies-orphans graduates
# from undetected (Tier D) to auto-fixed (Tier A). --repair prunes
# both orphan-shape rows (issue_id-orphan AND local-depends_on_id-
# orphan) via Op::DbExec while preserving rows with external:*
# depends_on_id (cross-repo refs).

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

orphan_count() {
  python3 <<'PY'
import sqlite3
conn = sqlite3.connect(".beads/beads.db")
cur = conn.cursor()
cur.execute(
    "SELECT COUNT(*) FROM dependencies "
    "WHERE issue_id NOT IN (SELECT id FROM issues) "
    "  OR (depends_on_id NOT LIKE 'external:%' "
    "      AND depends_on_id NOT IN (SELECT id FROM issues))"
)
print(cur.fetchone()[0])
conn.close()
PY
}

count_pair() {
  python3 - "$1" "$2" <<'PY'
import sqlite3, sys
owner, target = sys.argv[1], sys.argv[2]
conn = sqlite3.connect(".beads/beads.db")
cur = conn.cursor()
cur.execute(
    "SELECT COUNT(*) FROM dependencies "
    "WHERE issue_id = ? AND depends_on_id = ?",
    (owner, target),
)
print(cur.fetchone()[0])
conn.close()
PY
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "dependencies.orphans")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: dependencies.orphans not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "dependencies.orphans")' >&2
      exit 1
    }
    count=$(orphan_count)
    if [ "$count" != "2" ]; then
      echo "ASSERT FAIL[$stage]: expected 2 orphans, got '$count'" >&2
      exit 1
    fi
    jsonl_now=$(sha256sum .beads/issues.jsonl | awk '{print $1}')
    jsonl_pre=$(cat .fixture_jsonl_pre_sha256)
    if [ "$jsonl_now" != "$jsonl_pre" ]; then
      echo "ASSERT FAIL[$stage]: JSONL bytes drifted during corrupt stage" >&2
      exit 1
    fi
    ;;
  post_repair)
    count=$(orphan_count)
    if [ "$count" != "0" ]; then
      echo "ASSERT FAIL[$stage]: expected 0 orphans after repair, got '$count'" >&2
      exit 1
    fi
    if [ -f "$target_dir/_diag/repair.json" ]; then
      jq -e '
        .repaired == true
        and (.recovery_audit.applied_actions | index("dependencies_orphans_pruned"))
      ' "$target_dir/_diag/repair.json" >/dev/null || {
        echo "ASSERT FAIL[$stage]: repair output did not report dependencies_orphans_pruned" >&2
        cat "$target_dir/_diag/repair.json" >&2
        exit 1
      }
    fi
    # SACRED INVARIANT: JSONL byte-identical (dependencies orphans
    # aren't reachable from any issue so JSONL export is unaffected).
    jsonl_now=$(sha256sum .beads/issues.jsonl | awk '{print $1}')
    jsonl_pre=$(cat .fixture_jsonl_pre_sha256)
    if [ "$jsonl_now" != "$jsonl_pre" ]; then
      echo "ASSERT FAIL[$stage]: JSONL bytes mutated by --repair (sacred invariant violation)" >&2
      echo "  pre: $jsonl_pre" >&2
      echo "  now: $jsonl_now" >&2
      exit 1
    fi
    # Survivors retained.
    while IFS='|' read -r owner target; do
      [ -z "$owner" ] && continue
      retained=$(count_pair "$owner" "$target")
      if [ "$retained" != "1" ]; then
        echo "ASSERT FAIL[$stage]: survivor edge ($owner -> $target) not retained (count='$retained')" >&2
        exit 1
      fi
    done < .fixture_survivor_keys
    # External depends_on_id preserved specifically — read its
    # owner from .fixture_live_ids[2] (the third live id).
    external_owner=$(sed -n '3p' .fixture_live_ids)
    ext_retained=$(count_pair "$external_owner" "external:upstream-1")
    if [ "$ext_retained" != "1" ]; then
      echo "ASSERT FAIL[$stage]: external:upstream-1 ref not preserved" >&2
      exit 1
    fi
    redetect=$("$tool_bin" doctor --json 2>/dev/null) || true
    status=$(echo "$redetect" | jq -r '.checks[] | select(.name == "dependencies.orphans") | .status' 2>/dev/null || echo "")
    if [ "$status" != "ok" ] && [ -n "$status" ]; then
      echo "ASSERT FAIL[$stage]: dependencies.orphans still '$status' after repair" >&2
      exit 1
    fi
    runs_root="$target_dir/.doctor/runs"
    db_exec_count=0
    while IFS= read -r d; do
      a="$d/actions.jsonl"
      [ -s "$a" ] || continue
      c=$(grep -c '"op":"db_exec"' "$a" 2>/dev/null || echo 0)
      c="${c//[[:space:]]/}"
      db_exec_count=$((db_exec_count + ${c:-0}))
    done < <(find "$runs_root" -maxdepth 1 -mindepth 1 -type d 2>/dev/null)
    if [ "${db_exec_count:-0}" -lt 1 ]; then
      echo "ASSERT FAIL[$stage]: expected >=1 db_exec action, got $db_exec_count" >&2
      find "$runs_root" -name actions.jsonl -exec cat {} \; >&2 2>/dev/null || true
      exit 1
    fi
    ;;
  post_undo)
    count=$(orphan_count)
    if [ "$count" != "2" ]; then
      echo "ASSERT FAIL[$stage]: undo did not restore orphans; orphan count is '$count'" >&2
      exit 1
    fi
    # Verify each snapshotted orphan key is present byte-deterministically.
    while IFS='|' read -r owner target; do
      [ -z "$owner" ] && continue
      restored=$(count_pair "$owner" "$target")
      if [ "$restored" != "1" ]; then
        echo "ASSERT FAIL[$stage]: orphan edge ($owner -> $target) not restored (count='$restored')" >&2
        exit 1
      fi
    done < .fixture_orphan_keys
    jsonl_now=$(sha256sum .beads/issues.jsonl | awk '{print $1}')
    jsonl_pre=$(cat .fixture_jsonl_pre_sha256)
    if [ "$jsonl_now" != "$jsonl_pre" ]; then
      echo "ASSERT FAIL[$stage]: JSONL bytes drifted across undo" >&2
      exit 1
    fi
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac

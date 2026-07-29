#!/usr/bin/env bash
# Fixture assertions: dirty_bitmap_orphans
#
# Pass-5 cycle 29: fm-caches_indexes-dirty-bitmap-divergence
# graduates from detect-only (Tier B from cycle 8) to auto-fixed
# (Tier A). --repair issues a surgical DELETE via Op::DbExec with
# predicate-based snapshotting. JSONL bytes byte-unchanged throughout.

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
    "SELECT COUNT(*) FROM dirty_issues d "
    "LEFT JOIN issues i ON d.issue_id = i.id "
    "WHERE i.id IS NULL"
)
print(cur.fetchone()[0])
conn.close()
PY
}

orphan_key() {
  python3 <<'PY'
import sqlite3
conn = sqlite3.connect(".beads/beads.db")
cur = conn.cursor()
cur.execute("SELECT issue_id, marked_at FROM dirty_issues WHERE issue_id = 'bd-ghost-fixture-db'")
row = cur.fetchone()
print(f"{row[0]}|{row[1]}" if row else "")
conn.close()
PY
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "dirty_bitmap")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: dirty_bitmap not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "dirty_bitmap")' >&2
      exit 1
    }
    count=$(orphan_count)
    if [ "$count" != "1" ]; then
      echo "ASSERT FAIL[$stage]: expected 1 orphan, got '$count'" >&2
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
        and (.recovery_audit.applied_actions | index("dirty_bitmap_orphans_pruned"))
      ' "$target_dir/_diag/repair.json" >/dev/null || {
        echo "ASSERT FAIL[$stage]: repair output did not report dirty_bitmap_orphans_pruned" >&2
        cat "$target_dir/_diag/repair.json" >&2
        exit 1
      }
    fi
    # SACRED INVARIANT: JSONL byte-identical (orphan dirty_issues
    # rows aren't reachable from any issue so JSONL is unaffected).
    jsonl_now=$(sha256sum .beads/issues.jsonl | awk '{print $1}')
    jsonl_pre=$(cat .fixture_jsonl_pre_sha256)
    if [ "$jsonl_now" != "$jsonl_pre" ]; then
      echo "ASSERT FAIL[$stage]: JSONL bytes mutated by --repair (sacred invariant violation)" >&2
      echo "  pre: $jsonl_pre" >&2
      echo "  now: $jsonl_now" >&2
      exit 1
    fi
    redetect=$("$tool_bin" doctor --json 2>/dev/null) || true
    status=$(echo "$redetect" | jq -r '.checks[] | select(.name == "dirty_bitmap") | .status' 2>/dev/null || echo "")
    if [ "$status" != "ok" ] && [ -n "$status" ]; then
      echo "ASSERT FAIL[$stage]: dirty_bitmap still '$status' after repair" >&2
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
    if [ "$count" != "1" ]; then
      echo "ASSERT FAIL[$stage]: undo did not restore the orphan; orphan count is '$count'" >&2
      exit 1
    fi
    key=$(orphan_key)
    expected=$(cat .fixture_orphan_key)
    if [ "$key" != "$expected" ]; then
      echo "ASSERT FAIL[$stage]: restored key '$key' != snapshot '$expected'" >&2
      exit 1
    fi
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

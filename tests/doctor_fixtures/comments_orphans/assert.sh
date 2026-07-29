#!/usr/bin/env bash
# Fixture assertions: comments_orphans
#
# Pass-5 cycle 34: fm-caches_indexes-comments-orphans graduates from
# undetected (Tier D) to auto-fixed (Tier A). --repair issues a
# surgical DELETE via Op::DbExec with predicate-based snapshotting.
# The JSONL bytes are byte-unchanged throughout (comments orphans
# aren't reachable from any issue, so JSONL export is unaffected).

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
    "SELECT COUNT(*) FROM comments c "
    "LEFT JOIN issues i ON c.issue_id = i.id "
    "WHERE i.id IS NULL"
)
print(cur.fetchone()[0])
conn.close()
PY
}

orphan_body() {
  python3 <<'PY'
import sqlite3
conn = sqlite3.connect(".beads/beads.db")
cur = conn.cursor()
cur.execute("SELECT text FROM comments WHERE issue_id = 'bd-ghost-fixture'")
row = cur.fetchone()
print(row[0] if row else "")
conn.close()
PY
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "comments.orphans")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: comments.orphans not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "comments.orphans")' >&2
      exit 1
    }
    count=$(orphan_count)
    if [ "$count" != "1" ]; then
      echo "ASSERT FAIL[$stage]: expected 1 orphan, got '$count'" >&2
      exit 1
    fi
    # JSONL pre-state preserved.
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
        and (.recovery_audit.applied_actions | index("comments_orphans_pruned"))
      ' "$target_dir/_diag/repair.json" >/dev/null || {
        echo "ASSERT FAIL[$stage]: repair output did not report comments_orphans_pruned" >&2
        cat "$target_dir/_diag/repair.json" >&2
        exit 1
      }
    fi
    # SACRED INVARIANT: the JSONL bytes are byte-identical to the
    # pre-corruption state. Pruning unreachable orphan COMMENT rows
    # must NEVER touch JSONL.
    jsonl_now=$(sha256sum .beads/issues.jsonl | awk '{print $1}')
    jsonl_pre=$(cat .fixture_jsonl_pre_sha256)
    if [ "$jsonl_now" != "$jsonl_pre" ]; then
      echo "ASSERT FAIL[$stage]: JSONL bytes mutated by --repair (sacred invariant violation)" >&2
      echo "  pre: $jsonl_pre" >&2
      echo "  now: $jsonl_now" >&2
      exit 1
    fi
    # Re-detect: warning clears.
    redetect=$("$tool_bin" doctor --json 2>/dev/null) || true
    status=$(echo "$redetect" | jq -r '.checks[] | select(.name == "comments.orphans") | .status' 2>/dev/null || echo "")
    if [ "$status" != "ok" ] && [ -n "$status" ]; then
      echo "ASSERT FAIL[$stage]: comments.orphans still '$status' after repair" >&2
      exit 1
    fi
    # actions.jsonl records at least one db_exec op across the run dirs.
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
    # The orphan row reverts byte-deterministically.
    count=$(orphan_count)
    if [ "$count" != "1" ]; then
      echo "ASSERT FAIL[$stage]: undo did not restore the orphan; orphan count is '$count'" >&2
      exit 1
    fi
    body=$(orphan_body)
    expected=$(cat .fixture_orphan_body)
    if [ "$body" != "$expected" ]; then
      echo "ASSERT FAIL[$stage]: restored body '$body' != snapshot '$expected'" >&2
      exit 1
    fi
    # JSONL still byte-identical.
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

#!/usr/bin/env bash
# Fixture assertions: db_bloat

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

db_bloat_status() {
  local out
  out=$("$tool_bin" doctor --json 2>/dev/null) || true
  echo "$out" | jq -r '.checks[] | select(.name == "db_bloat") | .status'
}

assert_legacy_vacuum_action() {
  local runs_root="$target_dir/.doctor/runs"
  if ! grep -h '"fixer_id":"doctor.db_bloat_vacuum"' "$runs_root"/*/actions.jsonl 2>/dev/null \
    | grep -q '"op":"legacy_op"'; then
    echo "ASSERT FAIL[$stage]: no legacy_op action recorded for doctor.db_bloat_vacuum" >&2
    find "$runs_root" -name actions.jsonl -exec sh -c 'echo "--- $1 ---"; cat "$1"' sh {} \; >&2 2>/dev/null || true
    exit 1
  fi
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "db_bloat")
      | select(.status == "warn")
      | select((.details.ratio // 0) > (.details.threshold // 10))
      | select(.details.finding_id == "fm-caches_indexes-db-bloat-vs-jsonl")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: db_bloat did not warn with expected details" >&2
      echo "$out" | jq '.checks[] | select(.name == "db_bloat")' >&2
      exit 1
    }
    sqlite3 .beads/beads.db 'PRAGMA integrity_check;' | grep -Fxq ok
    ;;

  post_repair)
    if [ -f "$target_dir/_diag/repair.json" ]; then
      jq -e '
        .repaired == true
        and (.recovery_audit.applied_actions | index("db_bloat_vacuumed"))
        and (.report.checks[] | select(.name == "db_bloat") | .status == "ok")
      ' "$target_dir/_diag/repair.json" >/dev/null || {
        echo "ASSERT FAIL[$stage]: repair output did not report db_bloat_vacuumed" >&2
        cat "$target_dir/_diag/repair.json" >&2
        exit 1
      }
    fi

    db_bytes=$(wc -c < .beads/beads.db)
    pre_bytes=$(cat .fixture_db_bloat_pre_bytes)
    jsonl_bytes=$(cat .fixture_jsonl_bytes)
    max_ok=$((jsonl_bytes * 10))
    if [ "$db_bytes" -ge "$pre_bytes" ]; then
      echo "ASSERT FAIL[$stage]: VACUUM did not shrink DB ($db_bytes >= $pre_bytes)" >&2
      exit 1
    fi
    if [ "$db_bytes" -gt "$max_ok" ]; then
      echo "ASSERT FAIL[$stage]: DB remains above bloat threshold ($db_bytes > $max_ok)" >&2
      exit 1
    fi
    status=$(db_bloat_status)
    if [ "$status" != "ok" ]; then
      echo "ASSERT FAIL[$stage]: db_bloat status after repair is $status" >&2
      exit 1
    fi
    assert_legacy_vacuum_action
    ;;

  post_undo)
    expected=$(cat .fixture_db_bloat_pre_sha256)
    actual=$(sha256sum .beads/beads.db | awk '{print $1}')
    if [ "$actual" != "$expected" ]; then
      echo "ASSERT FAIL[$stage]: undo did not byte-restore bloated DB" >&2
      echo "  expected: $expected" >&2
      echo "  actual:   $actual" >&2
      exit 1
    fi
    status=$(db_bloat_status)
    if [ "$status" != "warn" ]; then
      echo "ASSERT FAIL[$stage]: db_bloat status after undo is $status" >&2
      exit 1
    fi
    ;;

  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac

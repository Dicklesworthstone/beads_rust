#!/usr/bin/env bash
# Fixture assertions: export_hash_cache_divergence
#
# Pass-4 cycle 4: fm-caches_indexes-export-hash-cache-divergence (top-level
# hash variant) graduates from undetected (Tier D) to auto-fixed (Tier A).
# --repair rewrites metadata.jsonl_content_hash to match the current
# JSONL bytes. The JSONL itself is byte-unchanged throughout.

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

read_metadata_hash() {
  python3 <<'PY'
import sqlite3
conn = sqlite3.connect(".beads/beads.db")
cur = conn.cursor()
cur.execute("SELECT value FROM metadata WHERE key='jsonl_content_hash'")
row = cur.fetchone()
print(row[0] if row else "")
conn.close()
PY
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "db.export_hash_cache")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: db.export_hash_cache not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "db.export_hash_cache")' >&2
      exit 1
    }
    stored=$(read_metadata_hash)
    if [ "$stored" != "deadbeef-poisoned" ]; then
      echo "ASSERT FAIL[$stage]: expected pre-fix stored hash 'deadbeef-poisoned', got '$stored'" >&2
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
    # Stored hash must now match the JSONL on disk.
    stored=$(read_metadata_hash)
    if [ "$stored" = "deadbeef-poisoned" ]; then
      echo "ASSERT FAIL[$stage]: stored hash still 'deadbeef-poisoned' after --repair" >&2
      exit 1
    fi
    if [ -f "$target_dir/_diag/repair.json" ]; then
      jq -e '
        .repaired == true
        and (.message | contains("metadata.jsonl_content_hash"))
        and (.recovery_audit.applied_actions | index("export_hash_cache_recomputed"))
      ' "$target_dir/_diag/repair.json" >/dev/null || {
        echo "ASSERT FAIL[$stage]: repair output did not report export-hash repair" >&2
        cat "$target_dir/_diag/repair.json" >&2
        exit 1
      }
    fi
    # SACRED INVARIANT: the JSONL bytes are byte-identical to the
    # pre-corruption state. The fixer must NEVER touch JSONL.
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
    status=$(echo "$redetect" | jq -r '.checks[] | select(.name == "db.export_hash_cache") | .status' 2>/dev/null || echo "")
    if [ "$status" != "ok" ] && [ -n "$status" ]; then
      echo "ASSERT FAIL[$stage]: db.export_hash_cache still '$status' after repair" >&2
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
    # The cache row reverts to the poisoned value.
    stored=$(read_metadata_hash)
    if [ "$stored" != "deadbeef-poisoned" ]; then
      echo "ASSERT FAIL[$stage]: undo did not restore poisoned hash; got '$stored'" >&2
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

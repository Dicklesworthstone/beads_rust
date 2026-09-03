#!/usr/bin/env bash
# Fixture assertions: wal_without_shm
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    # db.sidecars must be healthy since FrankenSQLite intentionally keeps its
    # WAL index in process-local memory rather than a sibling SHM file.
    # fsqlite 0.3.15 recreates `-shm` on every open, including the doctor's
    # own read-only inspection (verified 2026-09-03 with main's binary), so
    # the check reports either the WAL-only wording or "SHM sidecar ... is
    # inert beside the WAL". Both describe a healthy family; the contract is
    # status ok and a message that names the WAL.
    echo "$out" | jq -e '
      .checks[] | select(.name == "db.sidecars") | select(.status == "ok")
      | select(.message | test("WAL sidecar|inert beside the WAL"; "i"))
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: db.sidecars not healthy for valid WAL-without-SHM" >&2
      echo "$out" | jq '.checks[] | select(.name == "db.sidecars")' >&2
      exit 1
    }
    ;;
  post_repair)
    # Repair either no-ops or checkpoints the WAL into the DB. Either is
    # acceptable as long as data is preserved: beads.db must remain a real
    # SQLite file and be queryable. (Doctor must NOT delete the WAL without
    # first checkpointing — but a fresh-init workspace has no uncommitted
    # data so checkpoint-then-remove is benign.)
    [ -f .beads/beads.db ] || {
      echo "ASSERT FAIL[$stage]: beads.db missing after --repair" >&2
      exit 1
    }
    size=$(stat -c%s .beads/beads.db 2>/dev/null || stat -f%z .beads/beads.db)
    if [ "$size" -lt 1024 ]; then
      echo "ASSERT FAIL[$stage]: beads.db suspiciously small after --repair ($size bytes)" >&2
      exit 1
    fi
    # Doctor should still report ok schema after repair.
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    n_err=$(echo "$out" | jq '[.checks[] | select(.status == "error")] | length')
    if [ "$n_err" -ne 0 ]; then
      echo "ASSERT FAIL[$stage]: --repair introduced error checks" >&2
      echo "$out" | jq '.checks[] | select(.status == "error")' >&2
      exit 1
    fi
    ;;
  post_undo)
    [ -f .beads/beads.db ] || { echo "ASSERT FAIL[$stage]: beads.db gone" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac

#!/usr/bin/env bash
# Fixture assertions: jsonl_duplicate_ids
#
# Pass-5 cycle 33: fm-state_files-jsonl-duplicate-ids surfaces
# duplicate top-level `id` values in the JSONL. DETECT-ONLY by
# design — operator decides which copy is canonical.

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "jsonl.duplicate_ids")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: jsonl.duplicate_ids not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "jsonl.duplicate_ids")' >&2
      exit 1
    }
    # Details report exactly one distinct duplicate id with two
    # total duplicate records and the sample contains `bd-aaa`.
    echo "$out" | jq -e '
      .checks[] | select(.name == "jsonl.duplicate_ids")
      | (.details.distinct_duplicate_ids == 1)
        and (.details.total_duplicate_records == 2)
        and ((.details.sample_duplicate_ids | index("bd-aaa")) != null)
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: detector details mismatch" >&2
      echo "$out" | jq '.checks[] | select(.name == "jsonl.duplicate_ids") | .details' >&2
      exit 1
    }
    ;;
  post_repair)
    # SACRED INVARIANT: doctor must NOT mutate the JSONL. Auto-fix
    # would have to pick a winner among duplicates; that decision
    # belongs to the operator.
    jsonl_now=$(sha256sum .beads/issues.jsonl | awk '{print $1}')
    jsonl_pre=$(cat .fixture_jsonl_pre_sha256)
    if [ "$jsonl_now" != "$jsonl_pre" ]; then
      echo "ASSERT FAIL[$stage]: doctor silently mutated JSONL with duplicate ids (operator-intent violation)" >&2
      echo "  pre: $jsonl_pre" >&2
      echo "  now: $jsonl_now" >&2
      exit 1
    fi
    # Detector still warns post-repair because no fixer runs for
    # this FM. (Other fixers may have fired during --repair; we
    # only verify THIS check's state.)
    redetect=$("$tool_bin" doctor --json 2>/dev/null) || true
    status=$(echo "$redetect" | jq -r '.checks[] | select(.name == "jsonl.duplicate_ids") | .status' 2>/dev/null || echo "")
    if [ "$status" != "warn" ] && [ "$status" != "error" ]; then
      echo "ASSERT FAIL[$stage]: jsonl.duplicate_ids unexpectedly '$status' after repair" >&2
      exit 1
    fi
    ;;
  post_undo)
    # File still byte-identical (no mutation happened).
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

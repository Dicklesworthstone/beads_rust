#!/usr/bin/env bash
# Fixture assertions: multiple_br_in_path

set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

assert_stubs_unchanged() {
  local a_now b_now a_pre b_pre
  a_now="$(sha256sum bin_a/br | awk '{print $1}')"
  b_now="$(sha256sum bin_b/br | awk '{print $1}')"
  a_pre="$(cat .fixture_bin_a_sha256)"
  b_pre="$(cat .fixture_bin_b_sha256)"
  if [ "$a_now" != "$a_pre" ]; then
    echo "ASSERT FAIL[$stage]: bin_a/br was modified" >&2
    exit 1
  fi
  if [ "$b_now" != "$b_pre" ]; then
    echo "ASSERT FAIL[$stage]: bin_b/br was modified" >&2
    exit 1
  fi
}

assert_detector_fires() {
  local out
  out="$("$tool_bin" doctor --json 2>/dev/null || true)"
  echo "$out" | jq -e '
    .checks[]
    | select(.name == "br_path_dupes")
    | select(.status == "warn")
    | select(.details.finding_id == "fm-external_artifacts-multiple-br-in-path")
    | select((.details.br_paths // []) | length >= 2)
  ' >/dev/null || {
    echo "ASSERT FAIL[$stage]: br_path_dupes did not flag duplicate br executables" >&2
    echo "$out" | jq '.checks[] | select(.name == "br_path_dupes")' >&2
    exit 1
  }
}

case "$stage" in
  detect)
    assert_detector_fires
    assert_stubs_unchanged
    ;;
  post_repair)
    assert_detector_fires
    assert_stubs_unchanged
    if [ -d "$target_dir/.doctor/runs" ] \
      && find "$target_dir/.doctor/runs" -name actions.jsonl -print0 \
        | xargs -0 grep -E '"path":"([^"]*/)?bin_[ab]/br"' >/dev/null 2>&1; then
      echo "ASSERT FAIL[$stage]: repair actions touched synthetic br stubs" >&2
      find "$target_dir/.doctor/runs" -name actions.jsonl -exec cat {} \; >&2
      exit 1
    fi
    ;;
  post_undo)
    assert_detector_fires
    assert_stubs_unchanged
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac

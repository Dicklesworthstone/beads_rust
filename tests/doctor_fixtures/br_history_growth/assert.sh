#!/usr/bin/env bash
# Fixture assertions: br_history_growth

set -euo pipefail

target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"

cd "$target_dir"

expected_history="$(cat .fixture_expected_history)"
seed_history="$(cat .fixture_seed_history)"

history_count() {
  find .beads/.br_history -maxdepth 1 -type f -name 'issues.*.jsonl' 2>/dev/null | wc -l | tr -d '[:space:]'
}

assert_seed_history_intact() {
  if [ ! -f ".beads/.br_history/$seed_history" ]; then
    echo "ASSERT FAIL[$stage]: seed history snapshot vanished: $seed_history" >&2
    exit 1
  fi
  if ! grep -q '"id":"bd-history-001"' ".beads/.br_history/$seed_history"; then
    echo "ASSERT FAIL[$stage]: seed history snapshot content drifted" >&2
    exit 1
  fi
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "br_history.size")
      | select(.status == "warn")
      | select(.details.snapshot_count >= 105)
      | select(.details.threshold == 100)
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: br_history.size did not warn with expected details" >&2
      echo "$out" | jq '.checks[] | select(.name == "br_history.size")' >&2
      exit 1
    }
    assert_seed_history_intact
    ;;
  post_repair)
    current_count="$(history_count)"
    if [ "$current_count" -lt "$expected_history" ]; then
      echo "ASSERT FAIL[$stage]: --repair pruned history snapshots: expected >=$expected_history got $current_count" >&2
      exit 1
    fi
    assert_seed_history_intact
    ;;
  post_undo)
    current_count="$(history_count)"
    if [ "$current_count" -lt "$expected_history" ]; then
      echo "ASSERT FAIL[$stage]: undo pruned history snapshots: expected >=$expected_history got $current_count" >&2
      exit 1
    fi
    assert_seed_history_intact
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac


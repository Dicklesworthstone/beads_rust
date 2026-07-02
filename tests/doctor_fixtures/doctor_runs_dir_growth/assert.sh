#!/usr/bin/env bash
# Fixture assertions: doctor_runs_dir_growth

set -euo pipefail

target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"

cd "$target_dir"

expected_runs="$(cat .fixture_expected_runs)"
seed_run="$(cat .fixture_seed_run)"

run_count() {
  find .doctor/runs -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l | tr -d '[:space:]'
}

assert_seed_run_intact() {
  if [ ! -f ".doctor/runs/$seed_run/report.json" ]; then
    echo "ASSERT FAIL[$stage]: seed run report vanished: $seed_run" >&2
    exit 1
  fi
  if ! grep -q '"run_id":"seed01"' ".doctor/runs/$seed_run/report.json"; then
    echo "ASSERT FAIL[$stage]: seed run report content drifted" >&2
    exit 1
  fi
}

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "doctor.runs_dir")
      | select(.status == "warn")
      | select(.details.run_count >= 55)
      | select(.details.threshold == 50)
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: doctor.runs_dir did not warn with expected details" >&2
      echo "$out" | jq '.checks[] | select(.name == "doctor.runs_dir")' >&2
      exit 1
    }
    assert_seed_run_intact
    ;;
  post_repair)
    current_count="$(run_count)"
    if [ "$current_count" -lt "$expected_runs" ]; then
      echo "ASSERT FAIL[$stage]: --repair pruned run dirs: expected >=$expected_runs got $current_count" >&2
      exit 1
    fi
    assert_seed_run_intact
    ;;
  post_undo)
    current_count="$(run_count)"
    if [ "$current_count" -lt "$expected_runs" ]; then
      echo "ASSERT FAIL[$stage]: undo pruned run dirs: expected >=$expected_runs got $current_count" >&2
      exit 1
    fi
    assert_seed_run_intact
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac


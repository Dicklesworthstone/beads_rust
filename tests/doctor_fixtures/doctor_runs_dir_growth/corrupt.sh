#!/usr/bin/env bash
# Fixture: doctor_runs_dir_growth
# FM: fm-observability-doctor-runs-dir-grows-unbounded (P2)
#
# Plants enough `.doctor/runs/<run-id>/` directories for the
# `doctor.runs_dir` detector to warn. This is deliberately detect-only:
# old run artifacts are audit evidence and must not be pruned by --repair.

set -euo pipefail

target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"

"$tool_bin" init >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

# Avoid the pre-chokepoint .gitignore carveout adding unrelated noise during
# --repair. The fixture is about accumulated run dirs, not gitignore repair.
cat > .gitignore <<'EOF'
.doctor/
EOF

mkdir -p .doctor/runs

for i in $(seq -w 1 55); do
  run_dir=".doctor/runs/2024-01-01T00-00-${i}Z__seed${i}"
  mkdir -p "$run_dir"
  printf '{"schema_version":"br.doctor.report.v1","run_id":"seed%s","exit_code":0}\n' "$i" > "$run_dir/report.json"
  : > "$run_dir/actions.jsonl"
  touch -d "2024-01-01T00:00:00Z" "$run_dir" "$run_dir/report.json" "$run_dir/actions.jsonl"
done

printf '55\n' > .fixture_expected_runs
printf '2024-01-01T00-00-01Z__seed01\n' > .fixture_seed_run

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .


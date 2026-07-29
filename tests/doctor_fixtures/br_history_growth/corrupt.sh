#!/usr/bin/env bash
# Fixture: br_history_growth
# FM: fm-state_files-br-history-grows-unbounded (P2)

set -euo pipefail

target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"

"$tool_bin" init >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

# Avoid unrelated .doctor/.gitignore repair noise when --repair creates a run
# directory.
cat > .gitignore <<'EOF'
.doctor/
EOF

mkdir -p .beads/.br_history

for i in $(seq 1 105); do
  minute=$((i / 60))
  second=$((i % 60))
  stamp="$(printf '20240101_00%02d%02d_000000' "$minute" "$second")"
  label="$(printf '%03d' "$i")"
  backup=".beads/.br_history/issues.${stamp}.jsonl"
  printf '{"id":"bd-history-%s","title":"history snapshot %s"}\n' "$label" "$label" > "$backup"
  touch -d "2024-01-01T00:00:00Z" "$backup"
done

printf '105\n' > .fixture_expected_history
printf 'issues.20240101_000001_000000.jsonl\n' > .fixture_seed_history

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

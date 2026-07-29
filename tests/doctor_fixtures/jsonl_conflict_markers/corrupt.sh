#!/usr/bin/env bash
# Fixture: jsonl_conflict_markers
# FM: fm-state_files-jsonl-conflict-markers (P0) — detect-only in current binary
#
# Plants git merge conflict markers in .beads/issues.jsonl. Currently the
# refuse_gate / sync_conflict_markers check trips and `--repair` either refuses
# (exit 4 RefusedUnsafe) or leaves the conflict alone. Fixture asserts
# detection; post_repair asserts the markers remain (no destructive auto-fix
# without operator opt-in).

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

cat > .beads/issues.jsonl <<'EOF'
<<<<<<< HEAD
{"id":"test-1","title":"left","status":"open"}
=======
{"id":"test-1","title":"right","status":"open"}
>>>>>>> feature-branch
EOF

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

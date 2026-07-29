#!/usr/bin/env bash
# Fixture: gitignore_leaking_beads
# FM: fm-configs-gitignore-leaking-beads (P0)
#
# Plants a root `.gitignore` whose `.beads/` line hides the JSONL sync contract
# from git. This is the chokepointed gold-standard repair: detection is
# `gitignore.beads_inner` (warn), `--repair` rewrites .gitignore through the
# `mutate()` chokepoint, and `undo latest` restores byte-identically.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"

git init --quiet 2>/dev/null || true
"$tool_bin" init >/dev/null 2>&1

cat > .gitignore <<'EOF'
node_modules/
.beads/
*.tmp
EOF

# Baseline snapshot for the harness to verify detection-only stages stay
# byte-identical.
if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
# Use tar to capture the planted state for later round-trip diffs.
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

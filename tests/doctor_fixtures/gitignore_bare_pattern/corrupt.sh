#!/usr/bin/env bash
# Fixture: gitignore_bare_pattern
# Variant of fm-configs-gitignore-leaking-beads using the bare `.beads` (no
# trailing slash) pattern, which historically slipped past naive regex matchers
# that only looked for `.beads/`. Confirms the detector handles both shapes.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
git init --quiet 2>/dev/null || true
"$tool_bin" init >/dev/null 2>&1

cat > .gitignore <<'EOF'
*.log
.beads
build/
EOF

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

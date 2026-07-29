#!/usr/bin/env bash
# Fixture: inner_gitignore_append
# FM: fm-configs-gitignore-leaking-beads (P2, inner subset)
#
# Initialises a workspace, replaces .beads/.gitignore with a hand-
# written file that includes an operator-custom line and only ONE of
# the two canonical patterns (`*.tmp` present, `.write.lock` missing).
# Doctor's `gitignore.beads_inner_present` check should fire warn;
# `--repair` should append the missing canonical pattern via
# Op::AppendFile while preserving every existing line; `doctor undo`
# should byte-restore the incomplete state.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

"$tool_bin" create --title "fixture issue 1" --type task --priority 2 >/dev/null 2>&1
"$tool_bin" sync --flush-only >/dev/null 2>&1

# Capture JSONL bytes pre-corruption — Op::AppendFile on the gitignore
# must NEVER touch JSONL.
sha256sum .beads/issues.jsonl | awk '{print $1}' > .fixture_jsonl_pre_sha256

# Replace the auto-generated inner .gitignore with an incomplete file
# that has an operator-custom line plus only one canonical pattern.
cat > .beads/.gitignore <<'GITIGNORE'
# operator-custom rule
local-cache/
*.tmp
GITIGNORE

# Capture the incomplete bytes so post_undo can verify byte-
# deterministic restore.
sha256sum .beads/.gitignore | awk '{print $1}' > .fixture_inner_gitignore_pre_sha256
cp .beads/.gitignore .fixture_inner_gitignore_pre.txt

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

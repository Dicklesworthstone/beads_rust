#!/usr/bin/env bash
# Fixture: merge_artifact_stuck
# FM: fm-state_files-merge-artifact-stuck (P2) — detect-only in current binary
#
# Plants stuck `.beads/issues.{base,left,right}.jsonl` artifacts from an
# interrupted `br sync --merge`. Doctor's `jsonl.merge_artifacts` check goes
# warn. `--repair` does not currently auto-delete these (P3 cleanup gap),
# so the fixture asserts detection only.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

: > .beads/issues.base.jsonl
: > .beads/issues.left.jsonl
: > .beads/issues.right.jsonl

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

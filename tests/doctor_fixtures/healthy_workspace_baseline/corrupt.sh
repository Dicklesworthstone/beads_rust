#!/usr/bin/env bash
# Fixture: healthy_workspace_baseline (control)
#
# Plants a clean `br init` workspace with no failure. Asserts the doctor
# health surface returns healthy and that `--repair` is a no-op (idempotence).

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

#!/usr/bin/env bash
# Fixture: routes_target_missing
# FM: fm-routes_external-route-target-missing (P1) — detect-only.
#
# Plants a well-formed routes.jsonl where one route points at a path
# that doesn't exist on disk. The `routes_jsonl` shape check passes
# (the line is valid JSON with both required fields); the new
# `routes.targets` resolution check fails because the target
# directory isn't a beads workspace.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

# Two routes: one valid (".", points at the workspace's own .beads),
# one broken (relative path to a sibling that doesn't exist).
cat > .beads/routes.jsonl <<'JSONL'
{"prefix":"self-","path":"."}
{"prefix":"ghost-","path":"../never-existed-workspace"}
JSONL

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

#!/usr/bin/env bash
# Fixture: rust_log_noisy_breaks_json
# FM: fm-observability-rust-log-noisy-breaks-json (P1) — detect-only.
#
# The "corruption" here is a process-env condition: RUST_LOG set to a
# verbose level. The detector reads `std::env::var("RUST_LOG")` so
# the assertions run `br` with `RUST_LOG=debug` to trigger the warn.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

# Stash the corruption "recipe" so assert.sh can re-create it: the
# only state we plant is a marker file naming the env override we
# want assert.sh to use.
echo "debug" > .beads/.fixture_rust_log_level

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

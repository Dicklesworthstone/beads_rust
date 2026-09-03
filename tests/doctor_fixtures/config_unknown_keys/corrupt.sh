#!/usr/bin/env bash
# Fixture: config_unknown_keys
# FM: fm-configs-unknown-keys — detect-only.
#
# Plant a syntactically valid `.beads/config.yaml` whose top-level key is not
# in the config key registry (`br config schema`). br ignores unknown keys at
# load time, which is exactly how a typo silently disables a setting; the
# `config.unknown_keys` check surfaces it at warn level. --repair must NOT
# rewrite the operator's file.
set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

cat > .beads/config.yaml <<'YAML'
issue_prefix: "proj"
# A typo for `default_priority`: valid YAML, unknown to br.
defualt_priority: 1
YAML

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

#!/usr/bin/env bash
# Fixture: config_yaml_malformed
# FM: fm-configs-yaml-malformed (P1) — detect-only.
#
# Plant `.beads/config.yaml` with a syntactically-invalid block-
# mapping/sequence mix. serde_yml surfaces a parse error which the
# new pass-2 detector `check_config_yaml` re-emits at warn level.
# --repair must NOT silently rewrite the file.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

# `br init` may have left no config.yaml. Plant one with a
# deliberate parse error (a sequence item under a mapping at the
# same indent — YAML rejects this).
cat > .beads/config.yaml <<'YAML'
id:
  prefix: "proj"
  - bad
  invalid_block_mapping_entry
YAML

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

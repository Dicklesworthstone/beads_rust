#!/usr/bin/env bash
# Fixture: policy_unknown_keys
# FM: fm-configs-policy-unknown-keys — detect-only (GH #515).
#
# Plant a syntactically valid `.beads/policy.yaml` with a misspelled
# close-policy key. Since beads_rust#302 unknown policy keys are ignored
# instead of failing the load, which is exactly how a typo silently disables
# a guardrail; the `policy.unknown_keys` check surfaces it at warn level.
# --repair must NOT rewrite the operator's file.
set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

cat > .beads/policy.yaml <<'YAML'
close_policy:
  # A typo for `require_close_reason`: valid YAML, unknown to br.
  require_close_reasn: {enabled: true, min_length: 40}
YAML

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

#!/usr/bin/env bash
# Fixture: config_yaml_secret_mode
# FM: fm-permissions-config-yaml-mode-leaks-secrets (P1) — config.yaml
# is world-readable AND contains secret-shaped keywords.
#
# Initialises a workspace, writes a config.yaml with a token in it,
# then chmods to 0o666 to set the world-read+write bits. Doctor's
# `permissions.config_yaml_secrets` check should fire warn;
# `--repair` should strip the world-read+write bits via Op::Chmod;
# `doctor undo` should restore the original 0o666 mode.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

# Write a config.yaml containing a secret-shaped keyword so the
# detector fires. Use `token:` because the detector matches on
# token/secret/password/api_key/private_key substrings.
cat > .beads/config.yaml <<'YAML'
github_token: ghp_fixture_placeholder_REDACTED
project: fixture-workspace
YAML

# Set world-readable AND world-writable.
chmod 0666 .beads/config.yaml

# Capture pre-corruption bytes — Op::Chmod must NOT mutate content.
sha256sum .beads/config.yaml | awk '{print $1}' > .fixture_config_pre_sha256
# Snapshot the corrupted mode so post_undo can verify byte-
# deterministic restore.
python3 - .beads/config.yaml > .fixture_pre_mode <<'PY'
import os
import sys

print(format(os.stat(sys.argv[1]).st_mode & 0o777, "o"))
PY

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

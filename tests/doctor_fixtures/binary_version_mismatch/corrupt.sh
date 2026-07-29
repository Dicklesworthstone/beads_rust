#!/usr/bin/env bash
# Fixture: binary_version_mismatch
# FM: fm-external_artifacts-binary-version-mismatch (P1) — detect-only.
#
# Plant a Cargo.toml whose [package].name is "beads_rust" and whose
# version is FAR ahead of any binary version we'd ever ship (99.99.99).
# Run br doctor; the new pass-2 detector check_binary_version_mismatch
# walks upward from .beads/ and surfaces the drift.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

# Plant Cargo.toml in the parent of .beads/ (so the detector finds it
# via the upward walk). version=99.99.99 is guaranteed > any released
# binary.
cat > Cargo.toml <<'TOML'
[package]
name = "beads_rust"
version = "99.99.99"
edition = "2024"

[lib]
path = "/dev/null"
TOML

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

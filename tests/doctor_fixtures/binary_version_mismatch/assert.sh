#!/usr/bin/env bash
# Fixture assertions: binary_version_mismatch
set -euo pipefail
target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"
cd "$target_dir"

case "$stage" in
  detect)
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    echo "$out" | jq -e '
      .checks[] | select(.name == "binary_version")
      | select(.status == "warn" or .status == "error")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: binary_version not flagged" >&2
      echo "$out" | jq '.checks[] | select(.name == "binary_version")' >&2
      exit 1
    }
    # tree_version should be exactly "99.99.99" (what we planted).
    echo "$out" | jq -e '
      .checks[] | select(.name == "binary_version")
      | .details.tree_version == "99.99.99"
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: tree_version != 99.99.99" >&2
      echo "$out" | jq '.checks[] | select(.name == "binary_version") | .details' >&2
      exit 1
    }
    # recommended_fix must name `cargo install --path`.
    echo "$out" | jq -e '
      .checks[] | select(.name == "binary_version")
      | .details.recommended_fix | test("cargo install --path")
    ' >/dev/null || {
      echo "ASSERT FAIL[$stage]: recommended_fix missing canonical rebuild" >&2
      exit 1
    }
    ;;
  post_repair)
    # Detect-only — Cargo.toml must still be present and byte-identical.
    [ -f Cargo.toml ] || {
      echo "ASSERT FAIL[$stage]: Cargo.toml vanished after --repair (unsafe)" >&2
      exit 1
    }
    if ! grep -q 'version = "99.99.99"' Cargo.toml; then
      echo "ASSERT FAIL[$stage]: doctor silently edited the planted version" >&2
      cat Cargo.toml >&2
      exit 1
    fi
    ;;
  post_undo)
    [ -d .beads ] || { echo "ASSERT FAIL[$stage]: .beads gone after undo" >&2; exit 1; }
    [ -f Cargo.toml ] || { echo "ASSERT FAIL[$stage]: Cargo.toml gone after undo" >&2; exit 1; }
    ;;
  *)
    echo "unknown stage: $stage" >&2
    exit 2
    ;;
esac

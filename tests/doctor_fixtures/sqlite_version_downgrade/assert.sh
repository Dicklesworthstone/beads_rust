#!/usr/bin/env bash
# Fixture assertions: sqlite_version_downgrade

set -euo pipefail

target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"

cd "$target_dir"

current_db_sha() {
    sha256sum .beads/beads.db | cut -d ' ' -f 1
}

baseline_db_sha() {
    cut -d ' ' -f 1 .fixture_baseline/beads.db.sha256
}

header_user_version() {
    python3 - <<'PY'
from pathlib import Path

data = Path(".beads/beads.db").read_bytes()
print(int.from_bytes(data[60:64], "big"))
PY
}

assert_state_unchanged() {
    local expected_sha actual_sha version
    expected_sha="$(baseline_db_sha)"
    actual_sha="$(current_db_sha)"
    if [ "$expected_sha" != "$actual_sha" ]; then
        echo "ASSERT FAIL[$stage]: database bytes changed under schema-downgrade refusal" >&2
        echo "  expected: $expected_sha" >&2
        echo "  actual:   $actual_sha" >&2
        exit 1
    fi

    version="$(header_user_version)"
    if [ "$version" != "99" ]; then
        echo "ASSERT FAIL[$stage]: expected user_version 99, got $version" >&2
        exit 1
    fi
}

assert_repair_refuses() {
    local out exit_code
    set +e
    out=$("$tool_bin" doctor --repair --json 2>/dev/null)
    exit_code=$?
    set -e

    if [ "$exit_code" -ne 4 ]; then
        echo "ASSERT FAIL[$stage]: expected repair exit 4, got $exit_code" >&2
        echo "$out" >&2
        exit 1
    fi

    echo "$out" | jq -e '
      .exit_code == 4
      and .code == "refused_unsafe"
      and .gate == "schema_version_downgrade"
      and .evidence.db_schema_version == 99
      and (.evidence.binary_schema_version | type == "number")
      and (.recovery_audit.outcome == "refused")
    ' >/dev/null || {
        echo "ASSERT FAIL[$stage]: malformed schema downgrade refusal envelope" >&2
        echo "$out" | jq . >&2
        exit 1
    }
}

case "$stage" in
    detect)
        assert_state_unchanged
        assert_repair_refuses
        assert_state_unchanged
        ;;
    post_repair)
        assert_state_unchanged
        if [ -d .doctor/runs ]; then
            echo "ASSERT FAIL[$stage]: refuse gate created an unexpected repair run directory" >&2
            find .doctor/runs -maxdepth 2 -type f -print >&2
            exit 1
        fi
        ;;
    post_undo)
        assert_state_unchanged
        ;;
    *)
        echo "unknown stage: $stage" >&2
        exit 2
        ;;
esac

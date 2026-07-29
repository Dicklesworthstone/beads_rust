#!/usr/bin/env bash
# Fixture assertions: doctor_mutates_without_fix

set -euo pipefail

target_dir="${1:?usage: assert.sh <target_dir> <stage>}"
stage="${2:?usage: assert.sh <target_dir> <stage>}"
tool_bin="${TOOL_BIN:-br}"

cd "$target_dir"

snapshot_current_tree() {
    local prefix="${1:?prefix}"
    ( cd .beads && find . -type f -printf '%P\n' | sort ) > ".fixture_baseline/${prefix}.files"
    ( cd .beads && find . -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum | cut -d ' ' -f 1 ) \
        > ".fixture_baseline/${prefix}.sha256"
}

assert_tree_matches_snapshot() {
    local prefix="${1:?prefix}"
    local current_files=".fixture_baseline/${prefix}.current.files"
    local current_sha=".fixture_baseline/${prefix}.current.sha256"

    snapshot_current_tree "${prefix}.current"

    if ! diff -u ".fixture_baseline/${prefix}.files" "$current_files" >&2; then
        echo "ASSERT FAIL[$stage]: .beads file list changed" >&2
        exit 1
    fi
    if ! diff -u ".fixture_baseline/${prefix}.sha256" "$current_sha" >&2; then
        echo "ASSERT FAIL[$stage]: .beads file bytes changed" >&2
        exit 1
    fi
}

run_readonly_doctor_stably() {
    local prefix="${1:?prefix}"
    local out

    snapshot_current_tree "${prefix}.before"
    out=$("$tool_bin" doctor --json 2>/dev/null) || true
    snapshot_current_tree "${prefix}.after"

    if ! diff -u ".fixture_baseline/${prefix}.before.files" ".fixture_baseline/${prefix}.after.files" >&2; then
        echo "ASSERT FAIL[$stage]: read-only doctor changed .beads file list" >&2
        exit 1
    fi
    if ! diff -u ".fixture_baseline/${prefix}.before.sha256" ".fixture_baseline/${prefix}.after.sha256" >&2; then
        echo "ASSERT FAIL[$stage]: read-only doctor changed .beads file bytes" >&2
        exit 1
    fi

    printf '%s\n' "$out"
}

case "$stage" in
    detect)
        assert_tree_matches_snapshot beads
        out="$(run_readonly_doctor_stably detect)"
        assert_tree_matches_snapshot beads

        echo "$out" | jq -e '
          any(.checks[]; .name == "db.sidecars" and .status == "ok")
          and any(.checks[]; .name == "db.open" and .status == "error")
          and any(.checks[]; .name == "sqlite3.integrity_check" and .status == "error")
        ' >/dev/null || {
            echo "ASSERT FAIL[$stage]: malformed DB did not surface expected diagnostics" >&2
            echo "$out" | jq '.checks[] | select(.name == "db.sidecars" or .name == "db.open" or .name == "sqlite3.integrity_check")' >&2
            exit 1
        }
        ;;
    post_repair)
        run_readonly_doctor_stably post_repair >/dev/null
        ;;
    post_undo)
        run_readonly_doctor_stably post_undo >/dev/null
        ;;
    *)
        echo "unknown stage: $stage" >&2
        exit 2
        ;;
esac

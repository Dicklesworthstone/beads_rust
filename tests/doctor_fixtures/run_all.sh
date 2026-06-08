#!/usr/bin/env bash
# Phase 9: real-world fixture suite driver for `br doctor`.
#
# Iterates every subdirectory under this script's location, plants the
# failure via that dir's corrupt.sh, runs br doctor / br doctor --repair /
# br doctor undo latest, and checks each stage's assert.sh.
#
# Exit 0 if every fixture passes; non-zero (1) on the first failure with a
# clear diagnostic. Per-fixture isolation is provided by tempdir; the
# source tree is never mutated.
#
# Env:
#   TOOL_BIN  — path to the `br` binary (default: $CARGO_BIN_EXE_br, or
#               `cargo run --quiet --bin br --`)
#   FIXTURES_ROOT — override the fixtures directory
#   SKIP — space-separated fixture names to skip
#   ONLY — space-separated allowlist of fixture names; everything else skipped
#   FAIL_FAST — if "1" (default), exit on first failure; if "0", run all
#   REPLAY_IDEMPOTENCE — if "1", run --repair a second time and require
#                        newly-created replay run actions to be empty
#   REPLAY_IDEMPOTENCE_SKIP — space-separated fixture names to skip for replay

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURES_ROOT="${FIXTURES_ROOT:-$SCRIPT_DIR}"
FAIL_FAST="${FAIL_FAST:-1}"

if [ -z "${TOOL_BIN:-}" ]; then
    if [ -n "${CARGO_BIN_EXE_br:-}" ]; then
        TOOL_BIN="$CARGO_BIN_EXE_br"
    elif command -v br >/dev/null 2>&1; then
        TOOL_BIN="$(command -v br)"
    else
        echo "run_all.sh: cannot locate \`br\` binary (set TOOL_BIN or CARGO_BIN_EXE_br)" >&2
        exit 2
    fi
fi
export TOOL_BIN

if [ ! -x "$TOOL_BIN" ]; then
    echo "run_all.sh: TOOL_BIN=$TOOL_BIN is not executable" >&2
    exit 2
fi

if ! command -v jq >/dev/null 2>&1; then
    echo "run_all.sh: \`jq\` is required (apt-get install jq)" >&2
    exit 2
fi

# Running the full suite requires helper CLIs used by multiple fixture
# corrupt/assert scripts. When ONLY is set, let the selected fixture run so
# minimal one-off replays that do not need every helper still work.
if [ -z "${ONLY:-}" ]; then
    missing_tools=()
    for tool in python3 sqlite3 xxd; do
        if ! command -v "$tool" >/dev/null 2>&1; then
            missing_tools+=("$tool")
        fi
    done
    if [ "${#missing_tools[@]}" -gt 0 ]; then
        echo "run_all.sh: missing fixture tool(s): ${missing_tools[*]}" >&2
        echo "run_all.sh: install them or rerun with ONLY=<fixture> for a targeted replay" >&2
        exit 2
    fi
fi

declare -a fixtures=()
while IFS= read -r dir; do
    fixtures+=("$dir")
done < <(find "$FIXTURES_ROOT" -mindepth 1 -maxdepth 1 -type d | sort)

if [ "${#fixtures[@]}" -eq 0 ]; then
    echo "run_all.sh: no fixtures found under $FIXTURES_ROOT" >&2
    exit 2
fi

total=${#fixtures[@]}
pass=0
fail=0
skipped=0

contains() {
    local needle="$1"; shift
    for item in "$@"; do [ "$item" = "$needle" ] && return 0; done
    return 1
}

list_run_ids() {
    local runs_dir="$1"
    [ -d "$runs_dir" ] || return 0
    find "$runs_dir" -maxdepth 1 -mindepth 1 -type d -exec basename {} \; | sort
}

# Parse allowlist/blocklist
ONLY_LIST=()
SKIP_LIST=()
if [ -n "${ONLY:-}" ]; then
    read -ra ONLY_LIST <<<"$ONLY"
fi
if [ -n "${SKIP:-}" ]; then
    read -ra SKIP_LIST <<<"$SKIP"
fi

# Returns 0 on pass, 1 on fail, 2 on skip.
run_fixture() {
    local fixture_dir="$1"
    local name; name="$(basename "$fixture_dir")"

    if [ "${#ONLY_LIST[@]}" -gt 0 ] && ! contains "$name" "${ONLY_LIST[@]}"; then
        return 2
    fi
    if [ "${#SKIP_LIST[@]}" -gt 0 ] && contains "$name" "${SKIP_LIST[@]}"; then
        echo "[SKIP] $name (explicitly listed in SKIP)"
        return 2
    fi

    local corrupt_sh="$fixture_dir/corrupt.sh"
    local assert_sh="$fixture_dir/assert.sh"
    if [ ! -x "$corrupt_sh" ] || [ ! -x "$assert_sh" ]; then
        echo "[FAIL] $name: corrupt.sh or assert.sh missing/non-executable" >&2
        return 1
    fi

    local tmp; tmp="$(mktemp -d -t br-doctor-fixture-XXXXXX)"
    local diag="$tmp/_diag"
    mkdir -p "$diag"

    # `br doctor` walks parent directories to discover `.beads/`. We pin it
    # to the fixture tempdir by:
    #   1. cd-ing into $tmp before invoking br/scripts
    #   2. exporting BEADS_DIR=$tmp/.beads so even nested invocations honor it
    #   3. clearing BD_*, BEADS_*, RUST_LOG-noisy env that the developer's
    #      shell may have set
    # We do NOT use `env -i` because it strips PATH and PWD, and br needs PATH
    # to discover `git` for fixture-side `git init`.
    local doctor_env=(
        env
        HOME="$tmp"
        NO_COLOR=1
        RUST_LOG=error
        TOOL_BIN="$TOOL_BIN"
        BR_NO_AUTOFLUSH=1
        BEADS_DIR="$tmp/.beads"
        # Strip developer-shell beads overrides.
        --unset=BD_DB --unset=BD_DATABASE --unset=BEADS_DB
        --unset=BR_STARTUP_CACHE
    )

    # Stage 1: plant the failure.
    if ! ( cd "$tmp" && "${doctor_env[@]}" bash "$corrupt_sh" "$tmp" ) \
            > "$diag/corrupt.stdout" 2> "$diag/corrupt.stderr"; then
        echo "[FAIL] $name: corrupt stage failed" >&2
        sed 's/^/  /' "$diag/corrupt.stderr" >&2
        echo "  (workspace at $tmp)" >&2
        return 1
    fi

    # Stage 2: detect-stage assertions.
    if ! ( cd "$tmp" && "${doctor_env[@]}" bash "$assert_sh" "$tmp" detect ) \
            > "$diag/detect.stdout" 2> "$diag/detect.stderr"; then
        echo "[FAIL] $name: detect stage failed" >&2
        sed 's/^/  /' "$diag/detect.stderr" >&2
        echo "  (workspace at $tmp)" >&2
        return 1
    fi

    # Stage 3: --repair (don't abort on non-zero exit — assert.sh judges).
    local runs_dir="$tmp/.doctor/runs"
    local before_first_repair="$diag/runs_before_first_repair"
    list_run_ids "$runs_dir" > "$before_first_repair"
    ( cd "$tmp" && "${doctor_env[@]}" "$TOOL_BIN" doctor --repair --json ) \
        > "$diag/repair.json" 2> "$diag/repair.stderr" || true
    # Snapshot the run-ids created by the FIRST --repair. Under
    # REPLAY_IDEMPOTENCE=1 a second --repair below may add a
    # no-op run-dir that becomes the new "latest", so we resolve
    # the undo target explicitly against this pre-replay snapshot.
    local after_first_repair="$diag/runs_after_first_repair"
    list_run_ids "$runs_dir" > "$after_first_repair"
    local first_repair_run_id
    first_repair_run_id="$(comm -13 "$before_first_repair" "$after_first_repair" | tail -n1)"

    # Stage 3.5 (pass-3, opt-in): idempotence replay gate. The
    # chokepoint contract requires that running `--repair` twice
    # in a row is a no-op on the second invocation. A second run
    # that produces any non-empty actions.jsonl line means either
    # the detector is impure (mutates a side-channel) or the fixer
    # isn't idempotent.
    #
    # OPT-IN: REPLAY_IDEMPOTENCE=1 enables the gate. The
    # post-replay `undo` target below is resolved against the
    # FIRST repair's run-id (not `latest`), so fixtures whose
    # post_undo asserts byte-deterministic restore of the first
    # repair are robust against the no-op replay run becoming
    # the new "latest".
    #
    # CI / pass-3 idempotence-audit invocation:
    #   REPLAY_IDEMPOTENCE=1 \
    #   REPLAY_IDEMPOTENCE_SKIP="gitignore_leaking_beads gitignore_bare_pattern" \
    #   bash tests/doctor_fixtures/run_all.sh
    #
    # Per-fixture opt-out (independent of the suite-level gate):
    # drop a `.skip_replay` marker file inside the fixture dir.
    # Historically these were needed because stage-5 used `undo
    # latest`; the explicit-run-id resolution above makes most of
    # them unnecessary, but the opt-out is retained for fixtures
    # whose detectors are genuinely non-idempotent.
    if [ "${REPLAY_IDEMPOTENCE:-0}" = "1" ]; then
        local skip_replay=0
        if [ -n "${REPLAY_IDEMPOTENCE_SKIP:-}" ]; then
            local skip_item
            for skip_item in ${REPLAY_IDEMPOTENCE_SKIP}; do
                if [ "$skip_item" = "$name" ]; then
                    skip_replay=1
                    break
                fi
            done
        fi
        if [ -f "$fixture_dir/.skip_replay" ]; then
            skip_replay=1
        fi
        if [ "$skip_replay" -eq 0 ]; then
            local before_runs="$diag/replay_runs.before"
            local after_runs="$diag/replay_runs.after"
            list_run_ids "$runs_dir" > "$before_runs"
            ( cd "$tmp" && "${doctor_env[@]}" "$TOOL_BIN" doctor --repair --json ) \
                > "$diag/repair_replay.json" 2> "$diag/repair_replay.stderr" || true
            list_run_ids "$runs_dir" > "$after_runs"
            local new_run_ids=()
            mapfile -t new_run_ids < <(comm -13 "$before_runs" "$after_runs")
            local new_run_id
            for new_run_id in "${new_run_ids[@]}"; do
                local newest_run="$runs_dir/$new_run_id"
                if [ -f "$newest_run/actions.jsonl" ]; then
                    local replay_action_count
                    replay_action_count="$(grep -c -v '^[[:space:]]*$' "$newest_run/actions.jsonl" 2>/dev/null || echo 0)"
                    replay_action_count="${replay_action_count//[[:space:]]/}"
                    if [ "${replay_action_count:-0}" -gt 0 ]; then
                        echo "[FAIL] $name: idempotence replay failed — second --repair produced $replay_action_count action(s)" >&2
                        echo "  --- replay actions.jsonl ---" >&2
                        sed 's/^/  /' "$newest_run/actions.jsonl" >&2
                        echo "  (workspace at $tmp)" >&2
                        return 1
                    fi
                fi
            done
        fi
    fi

    # Stage 4: post_repair assertions.
    if ! ( cd "$tmp" && "${doctor_env[@]}" bash "$assert_sh" "$tmp" post_repair ) \
            > "$diag/post_repair.stdout" 2> "$diag/post_repair.stderr"; then
        echo "[FAIL] $name: post_repair stage failed" >&2
        sed 's/^/  /' "$diag/post_repair.stderr" >&2
        echo "  --- repair.json head ---" >&2
        head -c 1024 "$diag/repair.json" >&2 || true
        echo >&2
        echo "  (workspace at $tmp)" >&2
        return 1
    fi

    # Stage 5: undo the FIRST repair (best-effort). We pin to the
    # specific run-id captured before any replay so that a no-op
    # second --repair under REPLAY_IDEMPOTENCE=1 doesn't shadow
    # the meaningful run via the `latest` alias. If for any
    # reason we failed to capture a first-repair run-id (e.g., a
    # detector that emits zero findings), fall back to `latest`.
    local undo_target="${first_repair_run_id:-latest}"
    if [ -z "$undo_target" ]; then
        undo_target="latest"
    fi
    ( cd "$tmp" && "${doctor_env[@]}" "$TOOL_BIN" doctor undo "$undo_target" --json ) \
        > "$diag/undo.json" 2> "$diag/undo.stderr" || true

    # Stage 6: post_undo assertions.
    if ! ( cd "$tmp" && "${doctor_env[@]}" bash "$assert_sh" "$tmp" post_undo ) \
            > "$diag/post_undo.stdout" 2> "$diag/post_undo.stderr"; then
        echo "[FAIL] $name: post_undo stage failed" >&2
        sed 's/^/  /' "$diag/post_undo.stderr" >&2
        echo "  --- undo.json ---" >&2
        cat "$diag/undo.json" >&2 || true
        echo "  (workspace at $tmp)" >&2
        return 1
    fi

    echo "[PASS] $name (workspace retained: $tmp)"
    return 0
}

echo "run_all.sh: $total fixture(s) under $FIXTURES_ROOT"
echo "run_all.sh: TOOL_BIN=$TOOL_BIN"
echo

for fixture_dir in "${fixtures[@]}"; do
    rc=0
    run_fixture "$fixture_dir" || rc=$?
    case "$rc" in
        0) pass=$((pass+1)) ;;
        1)
            fail=$((fail+1))
            if [ "$FAIL_FAST" = "1" ]; then
                echo
                echo "Summary: pass=$pass fail=$fail skipped=$skipped of $total"
                exit 1
            fi
            ;;
        2) skipped=$((skipped+1)) ;;
        *) fail=$((fail+1)) ;;
    esac
done

echo
echo "Summary: pass=$pass fail=$fail skipped=$skipped of $total"
[ "$fail" -eq 0 ] || exit 1

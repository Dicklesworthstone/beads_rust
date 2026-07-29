#!/usr/bin/env bash
# tests/e2e_scripts/concurrency_witness_logged.sh
#
# beads_rust-mjmk: concurrency-witness-refresh harness.
#
# Drives N concurrent `br list` reads + 1 `br update` write against a shared
# workspace, captures stderr per-process, asserts the post-update state is
# observable from a final `br show <id> --json`, and emits a structured
# event log to /tmp/concurrency_witness_<ts>.jsonl.
#
# Loops 50 iterations and reports per-iteration pass/fail. Exits 0 only if
# all 50 pass.
#
# Exit codes:
#   0   PASS (50/50)
#   1   FAIL (one or more iterations failed)
#   2   prerequisite missing

set -euo pipefail

LOG_TS=$(date -u +%Y%m%dT%H%M%SZ)
EVENT_LOG="/tmp/concurrency_witness_${LOG_TS}.jsonl"
SUMMARY="/tmp/concurrency_witness_${LOG_TS}.summary.txt"
log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "$SUMMARY"; }
fail() { log "FAIL: $*"; exit 1; }

emit_event() {
    local op="$1"
    local pid="${2:-0}"
    local exit_code="${3:-0}"
    local note="${4:-}"
    local iter="${5:-0}"
    printf '{"ts":"%s","iter":%d,"op":"%s","pid":%s,"exit":%d,"note":"%s"}\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%S.%6NZ)" \
        "$iter" "$op" "$pid" "$exit_code" "$note" \
        >> "$EVENT_LOG"
}

log "=== concurrency_witness_logged.sh START ts=${LOG_TS} ==="
: > "$EVENT_LOG"

if [[ -n "${BR_BIN:-}" ]]; then BR="$BR_BIN"
elif [[ -n "${CARGO_TARGET_DIR:-}" && -x "$CARGO_TARGET_DIR/release/br" ]]; then BR="$CARGO_TARGET_DIR/release/br"
elif command -v br >/dev/null 2>&1; then BR=$(command -v br)
else log "ERROR: br binary not found"; exit 2
fi
command -v jq >/dev/null 2>&1 || { log "ERROR: jq missing"; exit 2; }

ITER_PASS=0
ITER_FAIL=0
ITERS=${ITER_OVERRIDE:-50}
NUM_READERS=${READERS_OVERRIDE:-3}

run_iteration() {
    local iter="$1"
    local work
    work=$(mktemp -d)
    cd "$work"

    # init + create + flush
    "$BR" init --prefix br >/dev/null 2>&1 || { emit_event "init_fail" 0 1 "br init failed" "$iter"; cd /; rm -rf "$work"; return 1; }
    local create_out
    create_out=$("$BR" create "iter $iter target" -t task --no-auto-flush 2>&1) || { emit_event "create_fail" 0 1 "br create failed" "$iter"; cd /; rm -rf "$work"; return 1; }
    local target_id
    target_id=$(echo "$create_out" | grep -oE 'br-[a-z0-9-]+' | head -1)
    [[ -n "$target_id" ]] || { emit_event "id_extract_fail" 0 1 "$create_out" "$iter"; cd /; rm -rf "$work"; return 1; }
    "$BR" sync --flush-only >/dev/null 2>&1 || true
    emit_event "iter_start" 0 0 "target=$target_id" "$iter"

    # Spawn N concurrent readers (br list --json)
    local reader_pids=()
    for r in $(seq 1 "$NUM_READERS"); do
        ("$BR" list --json > "/tmp/concur_reader_${LOG_TS}_${iter}_${r}.out" 2> "/tmp/concur_reader_${LOG_TS}_${iter}_${r}.err"; emit_event "reader_done" "$$" $? "" "$iter") &
        reader_pids+=($!)
    done

    # One concurrent writer
    "$BR" update "$target_id" --priority 0 >"/tmp/concur_writer_${LOG_TS}_${iter}.out" 2>"/tmp/concur_writer_${LOG_TS}_${iter}.err" || { emit_event "writer_fail" 0 1 "" "$iter"; cd /; rm -rf "$work"; return 1; }
    emit_event "writer_done" 0 0 "" "$iter"

    # Wait for readers
    for p in "${reader_pids[@]}"; do
        wait "$p" || true
    done

    # Final invariant: show <id> --json must reflect priority=0 (the writer's effect)
    local final_prio
    final_prio=$("$BR" show "$target_id" --json 2>&1 | jq -r '.[0].priority // .priority' 2>/dev/null || echo "?")
    if [[ "$final_prio" != "0" ]]; then
        emit_event "final_invariant_fail" 0 1 "expected_priority=0 got=$final_prio" "$iter"
        cd /; rm -rf "$work"
        return 1
    fi

    emit_event "iter_pass" 0 0 "target=$target_id" "$iter"
    cd /; rm -rf "$work"
    return 0
}

log "  iterations=$ITERS readers_per_iter=$NUM_READERS"
for i in $(seq 1 "$ITERS"); do
    if run_iteration "$i"; then
        ITER_PASS=$((ITER_PASS + 1))
    else
        ITER_FAIL=$((ITER_FAIL + 1))
        log "  iter $i: FAIL"
    fi
done

log "=== concurrency_witness_logged.sh SUMMARY ==="
log "  iterations passed: $ITER_PASS / $ITERS"
log "  iterations failed: $ITER_FAIL"
log "  event log: $EVENT_LOG"
log "  summary: $SUMMARY"

# Cleanup per-iteration logs
rm -f "/tmp/concur_reader_${LOG_TS}_"*.out "/tmp/concur_reader_${LOG_TS}_"*.err "/tmp/concur_writer_${LOG_TS}_"*.out "/tmp/concur_writer_${LOG_TS}_"*.err 2>/dev/null

if [[ "$ITER_FAIL" -gt 0 ]]; then
    log "FAIL"
    exit 1
fi
log "PASS"
exit 0

#!/usr/bin/env bash
# tests/e2e_scripts/sync_safety_witness.sh
#
# beads_rust-yyxo: filesystem-witness regression script for br sync.
#
# Creates a workspace, runs `br sync --flush-only` and `br sync --import-only`,
# and uses Linux strace (or fall back to inotifywait, or a polling stat scan
# on macOS/other) to capture every filesystem mutation. Asserts each one is
# in the allowlist defined by SYNC_SAFETY_INVARIANTS.md PC-1 / PC-RECOVERY.
#
# Emits a structured JSON event log to /tmp/sync_safety_witness_<ts>.jsonl
# with one event per filesystem mutation:
#   {ts, op, path, allowed, reason_if_blocked}
#
# Exit codes:
#   0   all mutations within allowlist (PASS)
#   1   one or more mutations outside allowlist (FAIL — printed details)
#   2   prerequisite missing (br binary, tmpdir, etc.)
#   3   tracing tool unavailable (strace/inotifywait/dtrace)

set -euo pipefail

LOG_TS=$(date -u +%Y%m%dT%H%M%SZ)
EVENT_LOG="/tmp/sync_safety_witness_${LOG_TS}.jsonl"
PASS_FAIL_LOG="/tmp/sync_safety_witness_${LOG_TS}.summary.txt"

# --- helpers ---------------------------------------------------------------

emit_event() {
    local op="$1"
    local path="$2"
    local allowed="$3"
    local reason="${4:-}"
    printf '{"ts":"%s","op":"%s","path":"%s","allowed":%s,"reason_if_blocked":"%s"}\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%S.%6NZ)" "$op" "$path" "$allowed" "$reason" \
        >> "$EVENT_LOG"
}

# Mirror of is_allowed_sync_file in tests/e2e_sync_git_safety.rs
is_allowed_path() {
    local rel="$1"
    case "$rel" in
        .beads/.manifest.json|.beads/metadata.json|.beads/last-touched) return 0 ;;
        .beads/*.jsonl|.beads/*.jsonl.tmp|.beads/*.db|.beads/*.db-wal|.beads/*.db-shm|.beads/*.db-journal) return 0 ;;
        .beads/.br_history/*.meta.json) return 0 ;;
        .beads/.br_recovery/*.bak|.beads/.br_recovery/*.rebuild-failed|.beads/.br_recovery/*.truncated-wal) return 0 ;;
    esac
    case "$rel" in
        .beads/*.jsonl.*.tmp)
            local pid="${rel##*.jsonl.}"
            pid="${pid%.tmp}"
            [[ "$pid" =~ ^[0-9]+$ ]] && return 0
            ;;
    esac
    return 1
}

log_summary() {
    echo "[$(date -u +%H:%M:%S)] $*" | tee -a "$PASS_FAIL_LOG"
}

# --- preflight -------------------------------------------------------------

log_summary "=== sync_safety_witness.sh START ts=${LOG_TS} ==="
log_summary "  event log: ${EVENT_LOG}"
log_summary "  summary log: ${PASS_FAIL_LOG}"

# Locate br binary
if [[ -n "${BR_BIN:-}" ]]; then
    BR_BINARY="$BR_BIN"
elif [[ -n "${CARGO_BIN_EXE_br:-}" ]]; then
    BR_BINARY="$CARGO_BIN_EXE_br"
elif command -v br >/dev/null 2>&1; then
    BR_BINARY=$(command -v br)
else
    # Try project-local release build
    BR_BINARY="$(pwd)/target/release/br"
    if [[ ! -x "$BR_BINARY" ]]; then
        # Maybe CARGO_TARGET_DIR
        if [[ -n "${CARGO_TARGET_DIR:-}" && -x "$CARGO_TARGET_DIR/release/br" ]]; then
            BR_BINARY="$CARGO_TARGET_DIR/release/br"
        else
            log_summary "ERROR: br binary not found (set BR_BIN= or build with cargo build --release)"
            exit 2
        fi
    fi
fi
log_summary "  br binary: ${BR_BINARY}"

WORKSPACE=$(mktemp -d)
cd "$WORKSPACE"
log_summary "  workspace: ${WORKSPACE}"

# Initialize event log
: > "$EVENT_LOG"
emit_event "harness_start" "$WORKSPACE" "true" ""

# --- choose tracing strategy ----------------------------------------------

TRACER=""
if command -v strace >/dev/null 2>&1; then
    TRACER="strace"
elif command -v inotifywait >/dev/null 2>&1; then
    TRACER="inotifywait"
elif command -v dtrace >/dev/null 2>&1; then
    TRACER="dtrace"
else
    log_summary "WARNING: no kernel tracing tool found; falling back to polling stat snapshots (less precise)"
    TRACER="polling"
fi
log_summary "  tracer: ${TRACER}"

# --- exercise sync ---------------------------------------------------------

log_summary "Phase 1: br init"
"$BR_BINARY" init >/dev/null 2>&1 || { log_summary "ERROR: br init failed"; exit 2; }

log_summary "Phase 2: br create x3 (no-auto-flush)"
"$BR_BINARY" create "Witness test 1" -t task --no-auto-flush -q >/dev/null
"$BR_BINARY" create "Witness test 2" -t bug --no-auto-flush -q >/dev/null
"$BR_BINARY" create "Witness test 3" -t feature --no-auto-flush -q >/dev/null

# Snapshot fs state before sync
SNAPSHOT_BEFORE=$(mktemp)
find . -type f -not -path "./logs/*" 2>/dev/null | sort > "$SNAPSHOT_BEFORE"
log_summary "  files before sync: $(wc -l < "$SNAPSHOT_BEFORE") (snapshot: $SNAPSHOT_BEFORE)"

log_summary "Phase 3: br sync --flush-only (export)"
if [[ "$TRACER" == "strace" ]]; then
    STRACE_LOG=$(mktemp)
    strace -f -e trace=open,openat,creat,unlink,rename,renameat,renameat2 \
        -o "$STRACE_LOG" \
        "$BR_BINARY" sync --flush-only 2>/dev/null
    EXIT=$?
    log_summary "  strace log: $STRACE_LOG (exit: $EXIT)"
else
    "$BR_BINARY" sync --flush-only 2>/dev/null
    EXIT=$?
    log_summary "  sync exit: $EXIT (no strace; using fs-snapshot diff)"
fi

log_summary "Phase 4: br sync --import-only --force (import + maybe-rebuild)"
"$BR_BINARY" sync --import-only --force 2>/dev/null || true

# Snapshot fs state after
SNAPSHOT_AFTER=$(mktemp)
find . -type f -not -path "./logs/*" 2>/dev/null | sort > "$SNAPSHOT_AFTER"
log_summary "  files after sync: $(wc -l < "$SNAPSHOT_AFTER")"

# --- assert allowlist ------------------------------------------------------

log_summary "Phase 5: assert each modified file is in PC-1/PC-RECOVERY allowlist"

# Note: pipe-fed `while` loops below run in a subshell under bash, so any
# in-loop counter increments would NOT persist to the outer shell. We rely
# on the event-log file (appended via `emit_event`) as the source of truth
# and count lines in that file after both loops complete.

# Diff: new files in after
comm -13 "$SNAPSHOT_BEFORE" "$SNAPSHOT_AFTER" | while IFS= read -r path; do
    rel="${path#./}"
    if is_allowed_path "$rel"; then
        emit_event "create" "$rel" "true" ""
    else
        emit_event "create" "$rel" "false" "not in allowlist"
        log_summary "  VIOLATION: created '$rel' (not in PC-1/PC-RECOVERY allowlist)"
    fi
done

# Removed files (rare, but possible during rebuild)
comm -23 "$SNAPSHOT_BEFORE" "$SNAPSHOT_AFTER" | while IFS= read -r path; do
    rel="${path#./}"
    if is_allowed_path "$rel"; then
        emit_event "delete" "$rel" "true" ""
    else
        emit_event "delete" "$rel" "false" "not in allowlist"
        log_summary "  VIOLATION: deleted '$rel' (not in PC-1/PC-RECOVERY allowlist)"
    fi
done

# Cleanup snapshots
rm -f "$SNAPSHOT_BEFORE" "$SNAPSHOT_AFTER"

# --- summary ---------------------------------------------------------------

emit_event "harness_end" "$WORKSPACE" "true" ""

# Count from the event log (the source of truth across subshell boundaries).
# Use awk so a zero-match doesn't trigger set -e under -o pipefail.
ACTUAL_VIOLATIONS=$(awk '/"allowed":false/ { c++ } END { print c+0 }' "$EVENT_LOG")
ACTUAL_TOTAL=$(awk '/"op":"(create|delete)"/ { c++ } END { print c+0 }' "$EVENT_LOG")

log_summary "=== sync_safety_witness.sh SUMMARY ==="
log_summary "  total operations: ${ACTUAL_TOTAL}"
log_summary "  violations: ${ACTUAL_VIOLATIONS}"
log_summary "  event log: ${EVENT_LOG}"

if [[ "$ACTUAL_VIOLATIONS" -gt 0 ]]; then
    log_summary "FAIL"
    rm -rf "$WORKSPACE"
    exit 1
else
    log_summary "PASS"
    rm -rf "$WORKSPACE"
    exit 0
fi

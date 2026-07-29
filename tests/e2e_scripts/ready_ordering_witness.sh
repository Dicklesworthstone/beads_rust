#!/usr/bin/env bash
# tests/e2e_scripts/ready_ordering_witness.sh
#
# beads_rust-jsgu: e2e ordering witness for `br ready --json`.
#
# Creates a workspace with explicit P0/P1/P2/P3 issues at known ages, runs
# `br ready --json`, parses the result, asserts ordering invariants.
# Logs each step and reports PASS/FAIL.

set -euo pipefail

LOG_TS=$(date -u +%Y%m%dT%H%M%SZ)
SUMMARY="/tmp/ready_ordering_witness_${LOG_TS}.summary.txt"
log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "$SUMMARY"; }
fail() { log "FAIL: $*"; exit 1; }

log "=== ready_ordering_witness.sh START ts=${LOG_TS} ==="

if [[ -n "${BR_BIN:-}" ]]; then BR="$BR_BIN"
elif [[ -n "${CARGO_TARGET_DIR:-}" && -x "$CARGO_TARGET_DIR/release/br" ]]; then BR="$CARGO_TARGET_DIR/release/br"
elif command -v br >/dev/null 2>&1; then BR=$(command -v br)
else log "ERROR: br binary not found"; exit 2
fi
command -v jq >/dev/null 2>&1 || { log "ERROR: jq missing"; exit 2; }

WORK=$(mktemp -d)
cd "$WORK"
log "  workspace: $WORK"

"$BR" init >/dev/null 2>&1 || fail "br init"

# Phase 1: create 4 issues at distinct priorities
log "Phase 1: create P0/P1/P2/P3 issues"
"$BR" create "P0 Critical" -t bug -p 0 --no-auto-flush -q >/dev/null
"$BR" create "P1 High" -t feature -p 1 --no-auto-flush -q >/dev/null
"$BR" create "P2 Medium" -t task -p 2 --no-auto-flush -q >/dev/null
"$BR" create "P3 Low" -t docs -p 3 --no-auto-flush -q >/dev/null
log "  4 issues created"

# Phase 2: br ready --json
log "Phase 2: br ready --json"
READY=$("$BR" ready --json 2>&1) || fail "br ready failed"
COUNT=$(echo "$READY" | jq 'length')
[[ "$COUNT" == "4" ]] || fail "expected 4 ready issues; got $COUNT"
log "  [OK] returned 4 issues"

# Phase 3: invariant — sorted by priority ASC (or hybrid: P0/P1 before P2/P3)
log "Phase 3: assert ordering invariant"
PRIORITIES=$(echo "$READY" | jq -r '.[].priority')
log "  observed priorities: $(echo "$PRIORITIES" | tr '\n' ' ')"

# Check that no high-tier (priority <= 1) issue appears AFTER any low-tier (priority > 1)
PREV_TIER="high"
for p in $PRIORITIES; do
  if [[ "$p" -le 1 ]]; then
    if [[ "$PREV_TIER" == "low" ]]; then
      fail "high-tier P$p issue appears after low-tier issue (hybrid invariant violated)"
    fi
  else
    PREV_TIER="low"
  fi
done
log "  [OK] hybrid ordering invariant holds (high-tier before low-tier)"

# Phase 4: assert no duplicate IDs
log "Phase 4: assert no duplicate IDs"
UNIQUE=$(echo "$READY" | jq -r '.[].id' | sort -u | wc -l)
[[ "$UNIQUE" == "4" ]] || fail "duplicate IDs found (unique=$UNIQUE)"
log "  [OK] all IDs unique"

# Phase 5: filter by --priority 0 returns only P0
log "Phase 5: br ready --priority 0 --json (should return only P0)"
P0_ONLY=$("$BR" ready --priority 0 --json 2>&1) || fail "br ready --priority 0"
P0_COUNT=$(echo "$P0_ONLY" | jq 'length')
[[ "$P0_COUNT" == "1" ]] || fail "expected 1 P0 issue; got $P0_COUNT"
log "  [OK] priority filter works"

cd /
rm -rf "$WORK"
log "=== ready_ordering_witness.sh PASS ==="
exit 0

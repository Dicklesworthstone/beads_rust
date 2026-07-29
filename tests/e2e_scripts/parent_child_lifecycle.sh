#!/usr/bin/env bash
# tests/e2e_scripts/parent_child_lifecycle.sh
#
# beads_rust-uelt: full lifecycle E2E for parent-child semantics under the
# new single-parent contract (commit 6ccbc3d6 fix(storage): reject second
# parent-child parent).
#
# Walks: create → dep add parent-child → dep add parent-child (must reject)
#         → dep remove → dep add parent-child (succeeds again) →
#         update --parent (atomic replace) → close parent → child becomes
#         ready.
#
# Exit codes:
#   0   PASS
#   1   FAIL (named expectation didn't hold)
#   2   prerequisite missing

set -euo pipefail

LOG_TS=$(date -u +%Y%m%dT%H%M%SZ)
SUMMARY_LOG="/tmp/parent_child_lifecycle_${LOG_TS}.summary.txt"
log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "$SUMMARY_LOG"; }
fail() { log "FAIL: $*"; exit 1; }

log "=== parent_child_lifecycle.sh START ts=${LOG_TS} ==="

if [[ -n "${BR_BIN:-}" ]]; then
    BR="$BR_BIN"
elif [[ -n "${CARGO_TARGET_DIR:-}" && -x "$CARGO_TARGET_DIR/release/br" ]]; then
    BR="$CARGO_TARGET_DIR/release/br"
elif command -v br >/dev/null 2>&1; then
    BR=$(command -v br)
else
    log "ERROR: br binary not found"
    exit 2
fi
command -v jq >/dev/null 2>&1 || { log "ERROR: jq not in PATH"; exit 2; }

WORK=$(mktemp -d)
cd "$WORK"
log "  workspace: $WORK"
log "  br: $BR"

extract_id() {
    grep -oE 'br-[a-z0-9-]+' <<<"$1" | head -1
}

# Phase 1: init + create three issues
log "Phase 1: br init --prefix br + create 3 issues"
"$BR" init --prefix br >/dev/null 2>&1 || fail "br init"
P_OUT=$("$BR" create "Parent A" -t epic --no-auto-flush 2>&1)
log "  parent create: $(echo "$P_OUT" | head -1)"
PARENT=$(extract_id "$P_OUT")
[[ -n "$PARENT" ]] || fail "extract parent ID from: $P_OUT"
log "  parent: $PARENT"

C_OUT=$("$BR" create "Child task" -t task --no-auto-flush 2>&1)
CHILD=$(extract_id "$C_OUT")
[[ -n "$CHILD" ]] || fail "extract child ID from: $C_OUT"
log "  child: $CHILD"

P2_OUT=$("$BR" create "Alternate Parent" -t epic --no-auto-flush 2>&1)
PARENT2=$(extract_id "$P2_OUT")
[[ -n "$PARENT2" ]] || fail "extract parent2 ID from: $P2_OUT"
log "  parent2: $PARENT2"

# Phase 2: add parent-child (succeeds)
log "Phase 2: br dep add $CHILD $PARENT --type parent-child (must succeed)"
"$BR" dep add "$CHILD" "$PARENT" --type parent-child >/dev/null 2>&1 || \
    fail "first parent-child dep should succeed"
log "  [OK] first parent-child added"

# Phase 3: second parent-child must FAIL with clear error
log "Phase 3: br dep add $CHILD $PARENT2 --type parent-child (must FAIL)"
SET_OUT=$("$BR" dep add "$CHILD" "$PARENT2" --type parent-child 2>&1) && \
    fail "second parent-child should have rejected; got success"
log "  [OK] second parent-child rejected"
log "    error message: $(echo "$SET_OUT" | head -3 | tr '\n' ' ')"
echo "$SET_OUT" | grep -qiE "$CHILD|parent" || \
    fail "rejection message should mention child ID or 'parent'; got: $SET_OUT"
log "  [OK] error message is operator-readable"

# Phase 4: remove parent-child
log "Phase 4: br dep remove $CHILD $PARENT (must succeed)"
"$BR" dep remove "$CHILD" "$PARENT" >/dev/null 2>&1 || \
    fail "dep remove should succeed"
log "  [OK] parent-child removed"

# Phase 5: re-add parent-child to a different parent — must succeed
log "Phase 5: br dep add $CHILD $PARENT2 --type parent-child (after remove)"
"$BR" dep add "$CHILD" "$PARENT2" --type parent-child >/dev/null 2>&1 || \
    fail "subsequent parent-child after remove should succeed"
log "  [OK] new parent-child accepted"

# Phase 6: atomic parent replace via `br update --parent`
log "Phase 6: br update $CHILD --parent $PARENT (atomic replace)"
"$BR" update "$CHILD" --parent "$PARENT" >/dev/null 2>&1 || \
    fail "atomic parent update should succeed"
NEW_PARENT_DEPS=$("$BR" show "$CHILD" --json | jq -r '.[0].dependencies // .dependencies | .[] | select(.dependency_type == "parent-child") | .id')
[[ "$NEW_PARENT_DEPS" == "$PARENT" ]] || \
    fail "atomic parent update did not stick: deps=$NEW_PARENT_DEPS"
log "  [OK] parent atomically replaced ($PARENT2 → $PARENT)"

# Phase 7: close child first, then parent → epic-close-eligible flow
log "Phase 7a: br close $CHILD (close child first; epic with open children rejects close)"
"$BR" close "$CHILD" --reason "child done" >/dev/null 2>&1 || fail "br close child"
log "  [OK] child closed"

log "Phase 7b: br close $PARENT --reason 'parent done' (now allowed with all children closed)"
"$BR" close "$PARENT" --reason "parent done" >/dev/null 2>&1 || fail "br close parent"
log "  [OK] parent closed (epic close-eligible passes)"

# Verify both states stuck
P_STATUS=$("$BR" show "$PARENT" --json | jq -r '.[0].status // .status')
[[ "$P_STATUS" == "closed" ]] || fail "parent status: $P_STATUS"
C_STATUS=$("$BR" show "$CHILD" --json | jq -r '.[0].status // .status')
[[ "$C_STATUS" == "closed" ]] || fail "child status: $C_STATUS"
log "  [OK] both child + parent are closed"

cd /
rm -rf "$WORK"
log "=== parent_child_lifecycle.sh PASS ==="
exit 0

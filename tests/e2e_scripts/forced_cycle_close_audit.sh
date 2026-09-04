#!/usr/bin/env bash
# tests/e2e_scripts/forced_cycle_close_audit.sh
#
# beads_rust-30ci: regression check that no closed bead has audit-suspect
# close_reason text WITHOUT a corresponding `audit-historical-cycle-close-*`
# label.
#
# Walks all closed beads in the current workspace's .beads/issues.jsonl,
# greps for forced-cycle-close patterns in close_reason, and asserts each
# match carries an audit-historical-cycle-close-* label.
#
# Exit codes:
#   0   PASS — no un-triaged offenders
#   1   FAIL — found offenders without historical label

set -euo pipefail

LOG_TS=$(date -u +%Y%m%dT%H%M%SZ)
SUMMARY="/tmp/forced_cycle_close_audit_${LOG_TS}.summary.txt"
log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "$SUMMARY"; }
fail() { log "FAIL: $*"; exit 1; }

log "=== forced_cycle_close_audit.sh START ts=${LOG_TS} ==="

JSONL="${BEADS_JSONL:-.beads/issues.jsonl}"
[[ -f "$JSONL" ]] || fail "JSONL not found: $JSONL"
command -v jq >/dev/null 2>&1 || fail "jq missing"

# The same literal hedge phrases as the doctor rule this script mirrors
# (`audit.suspect_close_reasons`, beads_rust-m3mi, src/cli/commands/doctor.rs
# check_suspect_close_reasons), matched case-insensitively. The earlier
# greedy `forced.*close.*cycle` regex also matched prose that merely named
# this script or the audit document in a close reason, which flagged the
# audit's own policy bead (30ci) and failed the gate on every hosted run.
PATTERN='forced close due to cycle|due to dep cycle|due to dependency cycle|temporarily closed|wip close'

OFFENDER_COUNT=0
OFFENDER_IDS=""

while IFS= read -r line; do
    # Extract close_reason from each closed bead
    CLOSE_REASON=$(echo "$line" | jq -r 'if .status == "closed" then .close_reason // "" else "" end' 2>/dev/null || echo "")
    [[ -z "$CLOSE_REASON" ]] && continue

    if echo "$CLOSE_REASON" | grep -qiE "$PATTERN"; then
        ID=$(echo "$line" | jq -r '.id')
        # Check for historical label
        HAS_LABEL=$(echo "$line" | jq -r '.labels // [] | map(select(startswith("audit-historical-cycle-close-"))) | length')
        if [[ "$HAS_LABEL" == "0" ]]; then
            OFFENDER_COUNT=$((OFFENDER_COUNT + 1))
            OFFENDER_IDS="$OFFENDER_IDS $ID"
            log "  OFFENDER: $ID — close_reason matches but no historical label"
        fi
    fi
done < "$JSONL"

log "=== SUMMARY ==="
log "  un-triaged offenders: $OFFENDER_COUNT"

if [[ "$OFFENDER_COUNT" -gt 0 ]]; then
    log "FAIL: offenders need either label or close_reason update:$OFFENDER_IDS"
    exit 1
fi
log "PASS"
exit 0

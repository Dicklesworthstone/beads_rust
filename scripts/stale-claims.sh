#!/usr/bin/env bash
# stale-claims.sh — surface hidden in-progress work.
#
# `br ready` excludes in_progress beads, so an abandoned claim hides work
# indefinitely. This gate asks `br coordination status` (contract
# br.coordination.v1, read-only, never runs git, never auto-reclaims) for
# every in-progress claim, prints one line per claim, and exits non-zero when
# any claim is not `fresh`: `stale_candidate`, `abandoned_likely`,
# `no_mail_snapshot` (idle past the threshold but no Agent Mail snapshot was
# supplied, so liveness is unknown), or `ambiguous`.
#
# Usage:
#   scripts/stale-claims.sh [--owner-kind swarm-agent|human|unknown] \
#       [--reservations <snapshot.json>] [--agents <snapshot.json>] [extra br coordination status flags]
#
# Pass Agent Mail reservation/agent snapshots when available so idle claims are
# classified as stale or abandoned instead of `no_mail_snapshot`.
#
# Exit codes:
#   0  every claim is fresh (or there are no claims)
#   1  at least one stale or abandoned claim
#   2  br is missing, no workspace, or the JSON could not be parsed
#
# Environment:
#   BR      path to the br binary (default: br on PATH)
#   RUST_LOG defaults to error so dependency logs do not pollute the table
set -uo pipefail

BR=${BR:-br}
export RUST_LOG=${RUST_LOG:-error}

if ! command -v "$BR" >/dev/null 2>&1; then
  echo "stale-claims: br binary not found (BR=$BR)" >&2
  exit 2
fi
if ! command -v jq >/dev/null 2>&1; then
  echo "stale-claims: jq is required" >&2
  exit 2
fi

json=$("$BR" coordination status --json "$@" 2>/dev/null) || {
  echo "stale-claims: br coordination status failed (not in a beads workspace?)" >&2
  exit 2
}

if ! printf '%s' "$json" | jq -e '.claims' >/dev/null 2>&1; then
  echo "stale-claims: unexpected JSON shape (no .claims array)" >&2
  exit 2
fi

total=$(printf '%s' "$json" | jq '.claims | length')
if [ "$total" -eq 0 ]; then
  echo "stale-claims: no in-progress claims"
  exit 0
fi

printf '%-24s %-18s %-12s %10s %-18s %s\n' ISSUE ASSIGNEE OWNER_KIND IDLE_MIN CLASSIFICATION ACTION
printf '%s' "$json" | jq -r '
  .claims[]
  | [ .issue.id,
      (.assessment.assignee // "-"),
      (.assessment.owner_kind // "-"),
      (.assessment.updated_age_minutes // 0 | tostring),
      (.assessment.classification // "-"),
      (.assessment.recommended_action // "-") ]
  | @tsv' \
| while IFS=$'\t' read -r id assignee kind idle class action; do
    printf '%-24s %-18s %-12s %10s %-18s %s\n' "$id" "$assignee" "$kind" "$idle" "$class" "$action"
  done

stale=$(printf '%s' "$json" | jq '[.claims[] | select(.assessment.classification != "fresh")] | length')
echo "stale-claims: $total claim(s), $stale stale/abandoned"
if [ "$stale" -gt 0 ]; then
  echo "stale-claims: reclaim protocol is in AGENTS.md (\"Stale Claims and Reclaiming Abandoned Work\")" >&2
  exit 1
fi
exit 0

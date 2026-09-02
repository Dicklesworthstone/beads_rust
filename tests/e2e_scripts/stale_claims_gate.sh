#!/usr/bin/env bash
# e2e witness for scripts/stale-claims.sh
#
# Scenario (all inside a throwaway workspace under $TMPDIR):
#   1. init, create two issues, claim one       -> gate exits 0 (fresh)
#   2. back-date the claimed issue's updated_at by three days via JSONL import
#                                                -> gate exits 1 and names the issue
# Prints every command and its exit code so a failure is diagnosable from the log.
set -uo pipefail

BR=${BR:-br}
export RUST_LOG=${RUST_LOG:-error}
export NO_COLOR=1
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
GATE="$here/../../scripts/stale-claims.sh"

work=$(mktemp -d "${TMPDIR:-/tmp}/br-stale-claims-XXXXXX")
cd "$work" || exit 2
echo "[stale_claims_gate] workspace=$work br=$($BR --version 2>/dev/null)"

fail() { echo "[stale_claims_gate] FAIL: $*" >&2; exit 1; }
run() { echo "[stale_claims_gate] \$ $*"; "$@"; local rc=$?; echo "[stale_claims_gate] rc=$rc"; return $rc; }

git init -q -b main . 2>/dev/null || true
run "$BR" init --prefix scg >/dev/null || fail "init"
A=$("$BR" create "claimed work" --json | jq -r .id)
B=$("$BR" create "free work" --json | jq -r .id)
[ -n "$A" ] && [ -n "$B" ] || fail "create"
run "$BR" update "$A" --status in_progress --assignee gate-test-agent >/dev/null || fail "claim"

echo "[stale_claims_gate] step 1: fresh claim must pass"
out=$(bash "$GATE" --owner-kind swarm-agent 2>&1); rc=$?
echo "$out"
[ "$rc" -eq 0 ] || fail "expected exit 0 for a fresh claim, got $rc"
echo "$out" | grep -q "$A" || fail "table should list $A"

echo "[stale_claims_gate] step 2: back-date the claim by 3 days"
# Import never lets an older JSONL timestamp overwrite a newer DB row, so the
# workspace is rebuilt from the back-dated JSONL through the missing-database
# recovery path: flush, edit the export, set the DB family aside (never
# deleted), and import into a fresh database.
run "$BR" sync --flush-only >/dev/null || fail "flush"
old=$(date -u -d '3 days ago' +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -v-3d +%Y-%m-%dT%H:%M:%SZ)
# Import normalizes updated_at to be no earlier than created_at, so both are moved.
jq -c --arg id "$A" --arg old "$old" 'if .id == $id then .updated_at = $old | .created_at = $old else . end' \
  .beads/issues.jsonl > .beads/issues.jsonl.tmp || fail "jq rewrite"
mv .beads/issues.jsonl.tmp .beads/issues.jsonl
mkdir -p .beads/.aside
for f in .beads/beads.db*; do [ -e "$f" ] && mv "$f" ".beads/.aside/${f#.beads/}"; done
run "$BR" sync --import-only >/dev/null || fail "import into fresh database"
now_ts=$("$BR" show "$A" --json | jq -r '.[0].updated_at')
echo "[stale_claims_gate] updated_at now: $now_ts (expected $old)"
[ "${now_ts:0:10}" = "${old:0:10}" ] || fail "rebuilt database did not preserve the back-dated updated_at"

out=$(bash "$GATE" --owner-kind swarm-agent 2>&1); rc=$?
echo "$out"
[ "$rc" -eq 1 ] || fail "expected exit 1 for a stale claim, got $rc"
echo "$out" | grep -q "$A" || fail "stale table should list $A"
# Without an Agent Mail snapshot br cannot tell an abandoned claim from a
# live-reserved one, so it reports `no_mail_snapshot` / `inspect_mail`; with a
# snapshot it reports stale_candidate / abandoned_likely. All of them mean the
# claim needs a human or agent to look, which is what the gate exists for.
echo "$out" | grep -Eq 'stale_candidate|abandoned_likely|no_mail_snapshot|ambiguous' || fail "classification should be non-fresh"

echo "[stale_claims_gate] PASS"

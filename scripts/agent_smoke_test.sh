#!/usr/bin/env bash
# agent_smoke_test.sh - Minimal agent-centric smoke tests for br output surfaces.
#
# Notes:
# - This script intentionally does NOT delete its temp workspace automatically.
# - It verifies JSON and TOON outputs can be parsed/decoded, and checks env precedence.

set -euo pipefail

log() { echo "[agent_smoke $(date +%H:%M:%S)] $*" >&2; }

# Keep output predictable unless the caller explicitly opts into verbose logs.
export RUST_LOG="${RUST_LOG:-error}"

need_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        log "Missing required command: $1"
        exit 1
    fi
}

need_cmd jq
need_cmd tru

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ -n "${BR_BIN:-}" ]]; then
    BR="$BR_BIN"
elif [[ -x "$ROOT/target/debug/br" ]]; then
    BR="$ROOT/target/debug/br"
elif [[ -x "$ROOT/target/release/br" ]]; then
    BR="$ROOT/target/release/br"
elif command -v br >/dev/null 2>&1; then
    # Fallback for environments where br is installed but the repo isn't built.
    BR="br"
else
    log "br binary not found. Build it with:"
    log "  CARGO_TARGET_DIR=target cargo build"
    exit 1
fi

WORKDIR="$(mktemp -d)"
log "Workspace: $WORKDIR"
log "NOTE: Workspace is left in place (no auto-delete)."

cd "$WORKDIR"

log "Init workspace"
"$BR" init --prefix smoke >/dev/null

log "Create 3 issues"
"$BR" create "Smoke one" --type task --priority 2 --description "Short desc" --json >/dev/null
"$BR" create "Smoke two" --type bug --priority 0 --json >/dev/null
"$BR" create "Smoke three" --type feature --priority 1 --json >/dev/null

ID1=$("$BR" list --format json --limit 1 | jq -r ".[0].id")

log "JSON: list/show parse"
"$BR" list --format json --limit 5 | jq -e "type==\"array\"" >/dev/null
"$BR" show "$ID1" --format json | jq -e 'if type=="array" then (.[0] | has("id") and has("title")) else (has("id") and has("title")) end' >/dev/null

log "TOON: list/show decode+parse"
"$BR" list --format toon --limit 5 | tru --decode | jq -e "type==\"array\"" >/dev/null
"$BR" show "$ID1" --format toon | tru --decode | jq -e 'if type=="array" then (.[0] | has("id") and has("title")) else (has("id") and has("title")) end' >/dev/null

log "Env: TOON_DEFAULT_FORMAT defaults output when --format not provided"
TOON_DEFAULT_FORMAT=toon "$BR" list --limit 1 | tru --decode | jq -e "type==\"array\"" >/dev/null

log "Env: BR_OUTPUT_FORMAT takes precedence over TOON_DEFAULT_FORMAT"
BR_OUTPUT_FORMAT=json TOON_DEFAULT_FORMAT=toon "$BR" list --limit 1 | jq -e "type==\"array\"" >/dev/null

log "Error envelope (stderr JSON)"
ERR_JSON="$WORKDIR/err.json"
set +e
"$BR" show bd-NOTEXIST --format json > /dev/null 2> "$ERR_JSON"
EC=$?
set -e
jq -e ".error.code == \"ISSUE_NOT_FOUND\"" "$ERR_JSON" >/dev/null
log "Exit code for not-found: $EC"
log "OK"

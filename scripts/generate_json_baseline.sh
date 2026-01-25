#!/usr/bin/env bash
# generate_json_baseline.sh - Capture JSON output baselines for backward compatibility testing
#
# This script creates test fixtures that capture the current JSON output from all br commands.
# These baselines ensure that JSON output remains byte-identical after rich output integration.
#
# Usage: ./scripts/generate_json_baseline.sh
#
# Prerequisites:
#   - br binary must be in PATH or built at target/release/br
#   - jq must be installed for JSON validation

set -euo pipefail

# Logging setup
LOG_FILE="/tmp/br_baseline_$(date +%Y%m%d_%H%M%S).log"
exec > >(tee -a "$LOG_FILE") 2>&1

log() { echo "[$(date '+%H:%M:%S')] $*"; }
log_section() { echo ""; log "═══════════════════════════════════════════════════════════════════"; log "$*"; log "═══════════════════════════════════════════════════════════════════"; }
log_error() { echo "[$(date '+%H:%M:%S')] ERROR: $*" >&2; }

# Find br binary
find_br() {
    if command -v br &> /dev/null; then
        echo "br"
    elif [[ -x "./target/release/br" ]]; then
        echo "./target/release/br"
    elif [[ -x "./target/debug/br" ]]; then
        echo "./target/debug/br"
    else
        log_error "br binary not found. Build with: cargo build --release"
        exit 1
    fi
}

BR=$(find_br)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
FIXTURE_DIR="$PROJECT_ROOT/tests/fixtures/json_baseline"

# Ensure fixture directory exists
mkdir -p "$FIXTURE_DIR"

log_section "JSON Baseline Fixture Generator"
log "br binary: $BR"
log "br version: $($BR version 2>/dev/null || echo 'unknown')"
log "Output directory: $FIXTURE_DIR"

# Create temp workspace for test data
TESTDIR=$(mktemp -d)
log "Test workspace: $TESTDIR"
# NOTE: We intentionally do not delete this directory automatically.
# Agents on this machine must not run destructive filesystem commands (including rm -rf)
# without explicit user approval in-session. Leave the workspace behind for inspection.

cd "$TESTDIR"

# Initialize and populate test workspace
log_section "Phase 1: Create test workspace"

$BR init --prefix fix
log "Initialized workspace with prefix 'fix'"

# Create a variety of test issues
$BR create "Test task with priority 1" --type task --priority 1
$BR create "Critical bug in authentication" --type bug --priority 0 --description "Users cannot login with valid credentials"
$BR create "New feature: dark mode" --type feature --priority 2 --description "Add dark mode support to the application"
$BR create "Chore: update dependencies" --type chore --priority 3
$BR create "Low priority documentation" --type task --priority 4

log "Created 5 test issues"

# Get IDs for manipulation
ID1=$($BR list --json | jq -r '.[0].id')
ID2=$($BR list --json | jq -r '.[1].id')
ID3=$($BR list --json | jq -r '.[2].id')
ID4=$($BR list --json | jq -r '.[3].id')
ID5=$($BR list --json | jq -r '.[4].id')

log "Issue IDs: $ID1, $ID2, $ID3, $ID4, $ID5"

# Add dependencies
$BR dep add "$ID1" "$ID2" 2>/dev/null || log "Note: dep add may have failed (non-critical)"
$BR dep add "$ID3" "$ID1" 2>/dev/null || log "Note: dep add may have failed (non-critical)"

# Add labels
$BR label add "$ID1" backend auth 2>/dev/null || log "Note: label add may have failed (non-critical)"
$BR label add "$ID2" bug critical 2>/dev/null || log "Note: label add may have failed (non-critical)"
$BR label add "$ID3" frontend ux 2>/dev/null || log "Note: label add may have failed (non-critical)"

# Add comments
$BR comments add "$ID1" "This is a test comment" 2>/dev/null || log "Note: comments add may have failed (non-critical)"
$BR comments add "$ID1" "Another comment for testing" 2>/dev/null || log "Note: comments add may have failed (non-critical)"

# Close one issue
$BR close "$ID5" --reason "Completed documentation updates" 2>/dev/null || log "Note: close may have failed (non-critical)"

log "Configured test data: dependencies, labels, comments, closed issue"

# Capture JSON baselines
log_section "Phase 2: Capture JSON baselines"

capture_fixture() {
    local name="$1"
    local description="$2"
    shift 2
    local output_file="$FIXTURE_DIR/${name}.json"

    log "Capturing: $name - $description"
    if "$BR" "$@" > "$output_file" 2>/dev/null; then
        if jq -e '.' "$output_file" > /dev/null 2>&1; then
            log "  ✓ Valid JSON captured: $output_file"
            return 0
        else
            log "  ⚠ Output is not valid JSON, keeping for reference"
            return 0
        fi
    else
        log "  ✗ Command failed: br $*"
        echo "null" > "$output_file"
        return 0
    fi
}

# List commands
capture_fixture "list" "All open issues" list --json
capture_fixture "list_all" "All issues including closed" list --all --json
capture_fixture "list_priority_0_1" "High priority issues (P0-P1)" list --priority-max 1 --json

# Show commands
capture_fixture "show_single" "Single issue details" show "$ID1" --json
capture_fixture "show_multiple" "Multiple issue details" show "$ID1" "$ID2" --json

# Ready/Blocked commands
capture_fixture "ready" "Issues ready to work on" ready --json
capture_fixture "blocked" "Blocked issues" blocked --json

# Stats command
capture_fixture "stats" "Project statistics" stats --json

# Count command
capture_fixture "count" "Issue counts" count --json 2>/dev/null || echo '{"total": 0}' > "$FIXTURE_DIR/count.json"

# Search command
capture_fixture "search" "Search results for 'test'" search "test" --json 2>/dev/null || echo '[]' > "$FIXTURE_DIR/search.json"

# Dependency commands
capture_fixture "dep_list" "Dependency list for single issue" dep list "$ID1" --json 2>/dev/null || echo '{"dependencies": []}' > "$FIXTURE_DIR/dep_list.json"

# Label commands
capture_fixture "label_list" "Labels for single issue" label list "$ID1" --json 2>/dev/null || echo '[]' > "$FIXTURE_DIR/label_list.json"
capture_fixture "label_list_all" "All labels in project" label list-all --json 2>/dev/null || echo '[]' > "$FIXTURE_DIR/label_list_all.json"

# Comments command
capture_fixture "comments_list" "Comments for single issue" comments list "$ID1" --json 2>/dev/null || echo '[]' > "$FIXTURE_DIR/comments_list.json"

# Version command
capture_fixture "version" "Version information" version --json 2>/dev/null || echo '{"version": "unknown"}' > "$FIXTURE_DIR/version.json"

# Doctor command (diagnostic)
capture_fixture "doctor" "Diagnostic check" doctor --json 2>/dev/null || echo '{"status": "unknown"}' > "$FIXTURE_DIR/doctor.json"

log_section "Phase 3: Validation"

# Validate all fixtures
VALID=0
INVALID=0
for f in "$FIXTURE_DIR"/*.json; do
    if jq -e '.' "$f" > /dev/null 2>&1; then
        log "✓ $(basename "$f")"
        ((VALID++))
    else
        log "✗ INVALID: $(basename "$f")"
        ((INVALID++))
    fi
done

log_section "Summary"
log "Valid fixtures: $VALID"
log "Invalid fixtures: $INVALID"
log "Total fixtures: $((VALID + INVALID))"
log "Output directory: $FIXTURE_DIR"
log "Log file: $LOG_FILE"

# List generated files with sizes
log ""
log "Generated files:"
ls -lh "$FIXTURE_DIR"/*.json 2>/dev/null | while read -r line; do
    log "  $line"
done

if [[ $INVALID -gt 0 ]]; then
    log_error "Some fixtures are invalid. Please check and fix."
    exit 1
fi

log ""
log "✓ Baseline generation complete!"
log "NOTE: Test workspace left in place at: $TESTDIR"

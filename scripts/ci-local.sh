#!/usr/bin/env bash
# Run CI checks locally before pushing.
# Mirrors .github/workflows/ci.yml steps.

set -euo pipefail

log() {
    echo -e "\033[32m->\033[0m $*"
}

error() {
    echo -e "\033[31mERR\033[0m $*" >&2
    exit 1
}

check_cmd() {
    local cmd="$1"
    if ! command -v "$cmd" &>/dev/null; then
        error "Required command not found: $cmd"
    fi
}

run_reliability_gates() {
    log "Reliability gates: failure-corpus replay and doctor/recovery postconditions"
    RUST_LOG="${RUST_LOG:-beads_rust=debug}" cargo test --test workspace_failure_replay -- --nocapture

    log "Reliability gates: crash-injection sync matrix"
    HARNESS_ARTIFACTS=1 NO_COLOR=1 cargo test --test e2e_sync_failure_injection -- --nocapture

    log "Reliability gates: long-lived single-workspace stress"
    BR_LONG_STRESS_ITERATIONS="${BR_LONG_STRESS_ITERATIONS:-8}" \
        HARNESS_ARTIFACTS=1 \
        NO_COLOR=1 \
        cargo test --test e2e_workspace_scenarios scenario_long_lived_single_workspace_stress_suite -- --nocapture

    log "Reliability gates: concurrent command-family integrity stress"
    HARNESS_ARTIFACTS=1 \
        NO_COLOR=1 \
        cargo test --test e2e_concurrency e2e_interleaved_command_families_preserve_workspace_integrity -- --nocapture
}

main() {
    check_cmd cargo

    log "Formatting"
    cargo fmt --all -- --check

    log "Clippy (all features)"
    cargo clippy --all-targets --all-features -- -D warnings

    log "Clippy (no default features)"
    cargo clippy --all-targets --no-default-features -- -D warnings

    log "Check (all targets)"
    cargo check --all-targets --all-features

    log "Tests (all features, sharded like ci.yml)"
    scripts/test-shard.sh all

    run_reliability_gates

    log "All local CI checks passed"
}

main "$@"

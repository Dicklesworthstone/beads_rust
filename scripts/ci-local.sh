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

    log "Tests (all features)"
    cargo test --all-features -- --nocapture

    log "Tests (no default features)"
    cargo test --no-default-features

    log "Doc tests"
    cargo test --doc

    log "All local CI checks passed"
}

main "$@"

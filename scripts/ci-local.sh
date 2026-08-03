#!/usr/bin/env bash
# Run CI checks locally before pushing.
#
# NOTE: this used to say "Mirrors .github/workflows/ci.yml steps" — that
# file does not exist. This repo has no CI workflows at all (.github holds
# only dependabot.yml), so this script IS the check, not a mirror of one.
# Nothing runs it automatically; somebody has to.
#
# Current state, if you run it top to bottom: the two clippy steps and the
# check step pass. The `cargo fmt --all -- --check` step FAILS on a
# long-standing backlog of unformatted files (pre-dating this note) and
# will stop the script before it reaches clippy, so pass over it or run
# the steps individually until that is dealt with separately.

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

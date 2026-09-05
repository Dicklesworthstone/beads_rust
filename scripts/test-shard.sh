#!/usr/bin/env bash
# Run one slice of the test suite so a CI job or an RCH worker fits its time
# budget. Membership is derived from file names, so a new tests/*.rs file
# always lands in exactly one shard without a manifest to maintain.
#
# Usage:
#   scripts/test-shard.sh <shard>       run one shard
#   scripts/test-shard.sh all           run every shard in sequence
#   scripts/test-shard.sh list          print the shard names
#   scripts/test-shard.sh show <shard>  print the test binaries in a shard
#
# Shards:
#   lib          unit tests, binaries, doc tests; plus unit tests without default features
#   e2e-a-l      tests/e2e_[a-l]*.rs
#   e2e-m-z      tests/e2e_[m-z]*.rs
#   storage      tests/storage_*.rs, proptest_*.rs, repro_*.rs, workflow_*.rs
#   misc         every other tests/*.rs except bench*.rs
#   bench        tests/bench*.rs (slow; scheduled runs only)
#
# Under RCH: `rch exec -- scripts/test-shard.sh e2e-a-l`.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

SHARDS=(lib e2e-a-l e2e-m-z storage misc bench)
CARGO_TEST=(cargo test --locked --all-features)

shard_files() {
    case "$1" in
        e2e-a-l) find tests -maxdepth 1 -name 'e2e_[a-l]*.rs' ;;
        e2e-m-z) find tests -maxdepth 1 -name 'e2e_[m-z]*.rs' ;;
        storage) find tests -maxdepth 1 \( -name 'storage_*.rs' -o -name 'proptest_*.rs' -o -name 'repro_*.rs' -o -name 'workflow_*.rs' -o -name 'linearizability_multiprocess.rs' \) ;;
        bench) find tests -maxdepth 1 -name 'bench*.rs' ;;
        misc)
            find tests -maxdepth 1 -name '*.rs' \
                ! -name 'e2e_*.rs' ! -name 'storage_*.rs' ! -name 'proptest_*.rs' \
                ! -name 'repro_*.rs' ! -name 'workflow_*.rs' ! -name 'bench*.rs' \
                ! -name 'linearizability_multiprocess.rs'
            ;;
        *) return 1 ;;
    esac | sort
}

shard_binaries() {
    shard_files "$1" | sed -e 's#^tests/##' -e 's#\.rs$##'
}

run_shard() {
    local shard="$1"
    echo "== shard: $shard =="
    case "$shard" in
        lib)
            "${CARGO_TEST[@]}" --lib --bins
            "${CARGO_TEST[@]}" --doc
            cargo test --locked --no-default-features --lib
            ;;
        e2e-a-l|e2e-m-z|storage|misc|bench)
            local args=()
            local name
            while IFS= read -r name; do
                [ -n "$name" ] && args+=(--test "$name")
            done < <(shard_binaries "$shard")
            if [ "${#args[@]}" -eq 0 ]; then
                echo "shard $shard has no test binaries" >&2
                return 1
            fi
            "${CARGO_TEST[@]}" "${args[@]}"
            ;;
        *)
            echo "unknown shard: $shard (one of: ${SHARDS[*]}, all)" >&2
            return 2
            ;;
    esac
}

case "${1:-}" in
    list) printf '%s\n' "${SHARDS[@]}" ;;
    show) shard_binaries "${2:?usage: test-shard.sh show <shard>}" ;;
    all)
        for shard in "${SHARDS[@]}"; do
            [ "$shard" = bench ] && continue
            run_shard "$shard"
        done
        ;;
    "") echo "usage: scripts/test-shard.sh <shard|all|list|show <shard>>" >&2; exit 2 ;;
    *) run_shard "$1" ;;
esac

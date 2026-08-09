#!/usr/bin/env bash

set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
readonly REPO_ROOT
readonly WRAPPER="$REPO_ROOT/scripts/with-build-limits.sh"

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

assert_contains() {
    local haystack="$1"
    local needle="$2"

    [[ "$haystack" == *"$needle"* ]] || fail "expected output to contain: $needle"
}

assert_not_contains() {
    local haystack="$1"
    local needle="$2"

    [[ "$haystack" != *"$needle"* ]] || fail "output unexpectedly contained: $needle"
}

assert_equals() {
    local actual="$1"
    local expected="$2"
    local label="$3"

    [[ "$actual" == "$expected" ]] || fail "$label: expected $expected, got $actual"
}

plan_value() {
    local plan="$1"
    local key="$2"

    sed -n "s/^${key}=//p" <<<"$plan"
}

help_output="$($WRAPPER --help)"

assert_contains "$help_output" "half of the CPUs available to the process"
assert_contains "$help_output" "half of effective memory"
assert_contains "$help_output" "BR_BUILD_CPU_SHARE"
assert_contains "$help_output" "BR_BUILD_MEMORY_MAX_SHARE"
assert_contains "$help_output" "BR_BUILD_MEMORY_HIGH_SHARE"
assert_contains "$help_output" "BR_BUILD_CARGO_JOBS"
assert_contains "$help_output" "BR_BUILD_CODEGEN_UNITS"
assert_contains "$help_output" "BR_BUILD_LTO"
assert_not_contains "$help_output" "CPUs 0-79"
assert_not_contains "$help_output" "64 GiB"
assert_not_contains "$help_output" "160-core"

printf 'PASS: portable help contract\n'

plan_output="$($WRAPPER --plan)"
available_cpu_count="$(plan_value "$plan_output" available_cpu_count)"
selected_cpu_count="$(plan_value "$plan_output" selected_cpu_count)"
cpu_quota_percent="$(plan_value "$plan_output" cpu_quota_percent)"
effective_memory_bytes="$(plan_value "$plan_output" effective_memory_bytes)"
memory_max_bytes="$(plan_value "$plan_output" memory_max_bytes)"
memory_high_bytes="$(plan_value "$plan_output" memory_high_bytes)"

expected_selected_cpu_count=$((available_cpu_count / 2))
if (( expected_selected_cpu_count < 1 )); then
    expected_selected_cpu_count=1
fi

assert_equals "$selected_cpu_count" "$expected_selected_cpu_count" "default selected CPU count"
assert_equals "$cpu_quota_percent" "$((selected_cpu_count * 100))" "CPU quota"
page_size="$(getconf PAGESIZE)"
expected_memory_max_bytes="$((effective_memory_bytes / 2 / page_size * page_size))"
expected_memory_high_bytes="$((expected_memory_max_bytes * 3 / 4 / page_size * page_size))"
assert_equals "$memory_max_bytes" "$expected_memory_max_bytes" "default memory maximum"
assert_equals "$memory_high_bytes" "$expected_memory_high_bytes" "default memory high watermark"
assert_equals "$((memory_max_bytes % page_size))" "0" "page-aligned memory maximum"
assert_equals "$((memory_high_bytes % page_size))" "0" "page-aligned memory high watermark"
assert_equals "$(plan_value "$plan_output" cargo_jobs)" "2" "default Cargo jobs"
assert_equals "$(plan_value "$plan_output" release_codegen_units)" "4" "default release codegen units"
assert_equals "$(plan_value "$plan_output" release_lto)" "false" "default release LTO"

printf 'PASS: default portable policy plan\n'

macos_output="$(
    # shellcheck disable=SC2329 # Exported command substitute used by the wrapper process.
    uname() {
        printf 'Darwin\n'
    }
    # shellcheck disable=SC2329 # Exported command substitute used by the wrapper process.
    sysctl() {
        case "${2:-}" in
            hw.logicalcpu) printf '8\n' ;;
            hw.memsize) printf '17179869184\n' ;;
            *) return 1 ;;
        esac
    }
    # shellcheck disable=SC2329 # Exported command substitute used by the wrapper process.
    nice() {
        [[ "${1:-}" == "-n" && "${2:-}" == "5" && "${3:-}" == "--" ]] || return 64
        printf 'nice_increment=5\n'
        shift 3
        "$@"
    }
    export -f uname sysctl nice

    "$WRAPPER" /usr/bin/env 2>&1
)"

assert_contains "$macos_output" "weaker reduced-priority isolation"
assert_contains "$macos_output" "nice_increment=5"
assert_contains "$macos_output" "CARGO_BUILD_JOBS=2"
assert_contains "$macos_output" "CARGO_PROFILE_RELEASE_CODEGEN_UNITS=4"
assert_contains "$macos_output" "CARGO_PROFILE_RELEASE_LTO=false"

printf 'PASS: macOS reduced-priority command contract\n'

override_plan="$(
    BR_BUILD_CPU_SHARE=1 \
        BR_BUILD_MEMORY_MAX_SHARE=0.25 \
        BR_BUILD_MEMORY_HIGH_SHARE=0.5 \
        BR_BUILD_CARGO_JOBS=3 \
        BR_BUILD_CODEGEN_UNITS=8 \
        BR_BUILD_LTO=thin \
        "$WRAPPER" --plan
)"

override_available_cpu_count="$(plan_value "$override_plan" available_cpu_count)"
override_effective_memory_bytes="$(plan_value "$override_plan" effective_memory_bytes)"
override_memory_max_bytes="$(plan_value "$override_plan" memory_max_bytes)"

assert_equals "$(plan_value "$override_plan" selected_cpu_count)" \
    "$override_available_cpu_count" "overridden selected CPU count"
expected_override_memory_max="$((override_effective_memory_bytes / 4 / page_size * page_size))"
expected_override_memory_high="$((expected_override_memory_max / 2 / page_size * page_size))"
assert_equals "$override_memory_max_bytes" "$expected_override_memory_max" \
    "overridden memory maximum"
assert_equals "$(plan_value "$override_plan" memory_high_bytes)" \
    "$expected_override_memory_high" "overridden memory high watermark"
assert_equals "$(plan_value "$override_plan" cargo_jobs)" "3" "overridden Cargo jobs"
assert_equals "$(plan_value "$override_plan" release_codegen_units)" "8" \
    "overridden release codegen units"
assert_equals "$(plan_value "$override_plan" release_lto)" "thin" "overridden release LTO"

for invalid_override in \
    'BR_BUILD_CPU_SHARE=' \
    'BR_BUILD_CPU_SHARE=1.01' \
    'BR_BUILD_MEMORY_MAX_SHARE=' \
    'BR_BUILD_MEMORY_MAX_SHARE=2' \
    'BR_BUILD_MEMORY_HIGH_SHARE=' \
    'BR_BUILD_MEMORY_HIGH_SHARE=1' \
    'BR_BUILD_CARGO_JOBS=' \
    'BR_BUILD_CARGO_JOBS=0' \
    'BR_BUILD_CODEGEN_UNITS=' \
    'BR_BUILD_CODEGEN_UNITS=many' \
    'BR_BUILD_LTO=' \
    'BR_BUILD_LTO=maybe'; do
    override_name="${invalid_override%%=*}"
    override_value="${invalid_override#*=}"
    if env "$override_name=$override_value" "$WRAPPER" --plan >/dev/null 2>&1; then
        fail "invalid override was accepted: $invalid_override"
    fi
done

printf 'PASS: validated policy overrides\n'

linux_plan="$($WRAPPER --plan)"
linux_output="$(
    # shellcheck disable=SC2329 # Exported command substitute used by the wrapper process.
    sudo() {
        [[ "${1:-}" == "-n" ]] || return 64
        shift
        "$@"
    }
    # shellcheck disable=SC2329 # Exported command substitute used by the wrapper process.
    systemd-run() {
        while (( $# > 0 )); do
            case "$1" in
                --setenv=*)
                    export "${1#--setenv=}"
                    ;;
                --property=*)
                    printf '%s\n' "$1"
                    ;;
                --)
                    shift
                    "$@"
                    return
                    ;;
            esac
            shift
        done
        return 64
    }
    export -f sudo systemd-run

    # shellcheck disable=SC2016 # The grandchild shell expands these variables.
    "$WRAPPER" /bin/bash -c \
        '/bin/sh -c '\''printf "grandchild_jobs=%s\\ngrandchild_codegen=%s\\ngrandchild_lto=%s\\n" "$CARGO_BUILD_JOBS" "$CARGO_PROFILE_RELEASE_CODEGEN_UNITS" "$CARGO_PROFILE_RELEASE_LTO"'\'''
)"

assert_contains "$linux_output" \
    "--property=AllowedCPUs=$(plan_value "$linux_plan" selected_cpu_set)"
assert_contains "$linux_output" \
    "--property=CPUQuota=$(plan_value "$linux_plan" cpu_quota_percent)%"
assert_contains "$linux_output" \
    "--property=MemoryMax=$(plan_value "$linux_plan" memory_max_bytes)"
assert_contains "$linux_output" \
    "--property=MemoryHigh=$(plan_value "$linux_plan" memory_high_bytes)"
assert_contains "$linux_output" "--property=MemorySwapMax=0"
assert_contains "$linux_output" "grandchild_jobs=2"
assert_contains "$linux_output" "grandchild_codegen=4"
assert_contains "$linux_output" "grandchild_lto=false"

printf 'PASS: Linux transient-cgroup command contract\n'

unsupported_output="$(
    # shellcheck disable=SC2329 # Exported command substitute used by the wrapper process.
    uname() {
        printf 'FreeBSD\n'
    }
    export -f uname

    if "$WRAPPER" /bin/true 2>&1; then
        fail "unsupported platform unexpectedly ran the command"
    fi
)"
assert_contains "$unsupported_output" "unsupported host for resource-limited builds: FreeBSD"

printf 'PASS: unsupported platform refusal\n'

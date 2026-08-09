#!/usr/bin/env bash
# Run a build or validation command with portable resource limits.

set -euo pipefail

HOST_OS="$(uname -s)"
readonly HOST_OS
readonly CPU_SHARE="${BR_BUILD_CPU_SHARE-0.5}"
readonly MEMORY_MAX_SHARE="${BR_BUILD_MEMORY_MAX_SHARE-0.5}"
readonly MEMORY_HIGH_SHARE="${BR_BUILD_MEMORY_HIGH_SHARE-0.75}"
readonly CARGO_JOBS="${BR_BUILD_CARGO_JOBS-2}"
readonly CODEGEN_UNITS="${BR_BUILD_CODEGEN_UNITS-4}"
readonly RELEASE_LTO="${BR_BUILD_LTO-false}"

AVAILABLE_CPU_SET=""
AVAILABLE_CPU_COUNT=""
SELECTED_CPU_SET=""
SELECTED_CPU_COUNT=""
CPU_QUOTA_PERCENT=""
PHYSICAL_MEMORY_BYTES=""
INHERITED_MEMORY_MAX_BYTES="max"
EFFECTIVE_MEMORY_BYTES=""
MEMORY_MAX_BYTES=""
MEMORY_HIGH_BYTES=""

error() {
    printf 'error: %s\n' "$*" >&2
}

validate_fraction() {
    local name="$1"
    local value="$2"
    local allow_one="$3"

    if [[ ! "$value" =~ ^(0([.][0-9]+)?|1([.]0+)?)$ ]] ||
        ! awk -v value="$value" -v allow_one="$allow_one" \
            'BEGIN { exit !(value > 0 && (value < 1 || (allow_one == 1 && value == 1))) }'; then
        if [[ "$allow_one" == "1" ]]; then
            error "$name must be a decimal fraction greater than 0 and no greater than 1"
        else
            error "$name must be a decimal fraction greater than 0 and less than 1"
        fi
        return 1
    fi
}

validate_positive_integer() {
    local name="$1"
    local value="$2"

    if [[ ! "$value" =~ ^[1-9][0-9]*$ ]]; then
        error "$name must be a positive integer"
        return 1
    fi
}

validate_overrides() {
    validate_fraction BR_BUILD_CPU_SHARE "$CPU_SHARE" 1
    validate_fraction BR_BUILD_MEMORY_MAX_SHARE "$MEMORY_MAX_SHARE" 1
    validate_fraction BR_BUILD_MEMORY_HIGH_SHARE "$MEMORY_HIGH_SHARE" 0
    validate_positive_integer BR_BUILD_CARGO_JOBS "$CARGO_JOBS"
    validate_positive_integer BR_BUILD_CODEGEN_UNITS "$CODEGEN_UNITS"

    case "$RELEASE_LTO" in
        false|true|thin|fat) ;;
        *)
            error "BR_BUILD_LTO must be false, true, thin, or fat"
            return 1
            ;;
    esac
}

validate_cpu_set() {
    local cpu_set="$1"

    if [[ ! "$cpu_set" =~ ^[0-9]+(-[0-9]+)?(,[0-9]+(-[0-9]+)?)*$ ]] ||
        ! awk -v cpu_set="$cpu_set" '
            BEGIN {
                part_count = split(cpu_set, parts, ",")
                for (i = 1; i <= part_count; i++) {
                    range_count = split(parts[i], range, "-")
                    if (range_count == 2 && range[1] > range[2]) {
                        exit 1
                    }
                }
            }
        '; then
        error "invalid effective CPU set reported by the operating system: $cpu_set"
        return 1
    fi
}

cpu_set_count() {
    local cpu_set="$1"

    awk -v cpu_set="$cpu_set" '
        BEGIN {
            count = 0
            part_count = split(cpu_set, parts, ",")
            for (i = 1; i <= part_count; i++) {
                range_count = split(parts[i], range, "-")
                if (range_count == 1) {
                    count++
                } else {
                    count += range[2] - range[1] + 1
                }
            }
            print count
        }
    '
}

select_cpu_set() {
    local cpu_set="$1"
    local desired_count="$2"

    awk -v cpu_set="$cpu_set" -v desired_count="$desired_count" '
        BEGIN {
            emitted = 0
            part_count = split(cpu_set, parts, ",")
            for (i = 1; i <= part_count && emitted < desired_count; i++) {
                range_count = split(parts[i], range, "-")
                first = range[1]
                last = range_count == 1 ? range[1] : range[2]
                for (cpu = first; cpu <= last && emitted < desired_count; cpu++) {
                    if (emitted > 0) {
                        printf ","
                    }
                    printf "%d", cpu
                    emitted++
                }
            }
            printf "\n"
        }
    '
}

scale_floor() {
    local value="$1"
    local share="$2"

    awk -v value="$value" -v share="$share" 'BEGIN { printf "%.0f\n", int(value * share) }'
}

align_down_to_page() {
    local value="$1"
    local page_size

    page_size="$(getconf PAGESIZE)"
    if [[ ! "$page_size" =~ ^[1-9][0-9]*$ ]]; then
        error "could not determine the operating-system page size"
        return 1
    fi

    printf '%s\n' "$((value / page_size * page_size))"
}

linux_cgroup_dir() {
    local relative_path
    relative_path="$(awk -F: '$1 == "0" { print $3; exit }' /proc/self/cgroup)"

    if [[ -z "$relative_path" || "$relative_path" != /* ]]; then
        error "Linux cgroup v2 membership is unavailable"
        return 1
    fi

    printf '/sys/fs/cgroup%s\n' "$relative_path"
}

linux_inherited_memory_max() {
    local cgroup_dir="$1"
    local current="$cgroup_dir"
    local limit="max"
    local value

    while [[ "$current" == /sys/fs/cgroup* ]]; do
        if [[ -r "$current/memory.max" ]]; then
            value="$(<"$current/memory.max")"
            if [[ "$value" != "max" ]]; then
                if [[ ! "$value" =~ ^[0-9]+$ ]]; then
                    error "invalid inherited memory.max value: $value"
                    return 1
                fi
                if [[ "$limit" == "max" || "$value" -lt "$limit" ]]; then
                    limit="$value"
                fi
            fi
        fi

        [[ "$current" == "/sys/fs/cgroup" ]] && break
        current="${current%/*}"
    done

    printf '%s\n' "$limit"
}

discover_linux_authority() {
    local cgroup_dir
    local memory_kib

    if [[ ! -r /sys/fs/cgroup/cgroup.controllers ]]; then
        error "resource-limited builds require Linux cgroup v2"
        return 1
    fi

    AVAILABLE_CPU_SET="$(awk '$1 == "Cpus_allowed_list:" { print $2; exit }' /proc/self/status)"
    validate_cpu_set "$AVAILABLE_CPU_SET"

    memory_kib="$(awk '$1 == "MemTotal:" { print $2; exit }' /proc/meminfo)"
    if [[ ! "$memory_kib" =~ ^[0-9]+$ ]]; then
        error "could not determine physical memory from /proc/meminfo"
        return 1
    fi
    PHYSICAL_MEMORY_BYTES=$((memory_kib * 1024))

    cgroup_dir="$(linux_cgroup_dir)"
    INHERITED_MEMORY_MAX_BYTES="$(linux_inherited_memory_max "$cgroup_dir")"
}

discover_macos_authority() {
    local logical_cpus

    logical_cpus="$(sysctl -n hw.logicalcpu)"
    PHYSICAL_MEMORY_BYTES="$(sysctl -n hw.memsize)"
    if [[ ! "$logical_cpus" =~ ^[1-9][0-9]*$ || ! "$PHYSICAL_MEMORY_BYTES" =~ ^[1-9][0-9]*$ ]]; then
        error "could not determine macOS CPU and memory authority"
        return 1
    fi

    AVAILABLE_CPU_SET="0-$((logical_cpus - 1))"
    INHERITED_MEMORY_MAX_BYTES="max"
}

calculate_policy() {
    validate_overrides

    case "$HOST_OS" in
        Linux) discover_linux_authority ;;
        Darwin) discover_macos_authority ;;
        *)
            error "unsupported host for resource-limited builds: $HOST_OS"
            return 1
            ;;
    esac

    AVAILABLE_CPU_COUNT="$(cpu_set_count "$AVAILABLE_CPU_SET")"
    SELECTED_CPU_COUNT="$(scale_floor "$AVAILABLE_CPU_COUNT" "$CPU_SHARE")"
    if (( SELECTED_CPU_COUNT < 1 )); then
        SELECTED_CPU_COUNT=1
    fi
    SELECTED_CPU_SET="$(select_cpu_set "$AVAILABLE_CPU_SET" "$SELECTED_CPU_COUNT")"
    CPU_QUOTA_PERCENT=$((SELECTED_CPU_COUNT * 100))

    EFFECTIVE_MEMORY_BYTES="$PHYSICAL_MEMORY_BYTES"
    if [[ "$INHERITED_MEMORY_MAX_BYTES" != "max" &&
        "$INHERITED_MEMORY_MAX_BYTES" -lt "$EFFECTIVE_MEMORY_BYTES" ]]; then
        EFFECTIVE_MEMORY_BYTES="$INHERITED_MEMORY_MAX_BYTES"
    fi

    MEMORY_MAX_BYTES="$(align_down_to_page "$(scale_floor "$EFFECTIVE_MEMORY_BYTES" "$MEMORY_MAX_SHARE")")"
    MEMORY_HIGH_BYTES="$(align_down_to_page "$(scale_floor "$MEMORY_MAX_BYTES" "$MEMORY_HIGH_SHARE")")"
    if (( MEMORY_MAX_BYTES < 1 || MEMORY_HIGH_BYTES < 1 || MEMORY_HIGH_BYTES >= MEMORY_MAX_BYTES )); then
        error "memory shares produce invalid byte limits for the effective authority"
        return 1
    fi
}

print_plan() {
    calculate_policy

    printf 'os=%s\n' "$HOST_OS"
    if [[ "$HOST_OS" == "Linux" ]]; then
        printf 'isolation=transient-cgroup\n'
    else
        printf 'isolation=reduced-priority\n'
    fi
    printf 'available_cpu_set=%s\n' "$AVAILABLE_CPU_SET"
    printf 'available_cpu_count=%s\n' "$AVAILABLE_CPU_COUNT"
    printf 'selected_cpu_set=%s\n' "$SELECTED_CPU_SET"
    printf 'selected_cpu_count=%s\n' "$SELECTED_CPU_COUNT"
    printf 'cpu_quota_percent=%s\n' "$CPU_QUOTA_PERCENT"
    printf 'physical_memory_bytes=%s\n' "$PHYSICAL_MEMORY_BYTES"
    printf 'inherited_memory_max_bytes=%s\n' "$INHERITED_MEMORY_MAX_BYTES"
    printf 'effective_memory_bytes=%s\n' "$EFFECTIVE_MEMORY_BYTES"
    printf 'memory_max_bytes=%s\n' "$MEMORY_MAX_BYTES"
    printf 'memory_high_bytes=%s\n' "$MEMORY_HIGH_BYTES"
    printf 'cargo_jobs=%s\n' "$CARGO_JOBS"
    printf 'release_codegen_units=%s\n' "$CODEGEN_UNITS"
    printf 'release_lto=%s\n' "$RELEASE_LTO"
}

resolve_command() {
    local requested_command="$1"
    local command_dir
    local command_name

    if [[ "$requested_command" == */* ]]; then
        command_dir="$(cd -- "$(dirname -- "$requested_command")" && pwd -P)"
        command_name="$(basename -- "$requested_command")"
        printf '%s/%s\n' "$command_dir" "$command_name"
    else
        type -P -- "$requested_command" || true
    fi
}

check_inside_cgroup() {
    local cgroup_dir
    local cgroup_relative_path

    cgroup_relative_path="$(awk -F: '$1 == "0" { print $3; exit }' /proc/self/cgroup)"
    cgroup_dir="$(linux_cgroup_dir)"

    for control_file in cpuset.cpus.effective cpu.max memory.high memory.max memory.swap.max; do
        if [[ ! -r "$cgroup_dir/$control_file" ]]; then
            error "active cgroup does not expose $control_file"
            return 1
        fi
    done

    printf 'Limited command environment:\n'
    printf '  isolation: Linux transient cgroup\n'
    printf '  cgroup: %s\n' "$cgroup_relative_path"
    printf '  CPUs allowed: %s\n' "$(<"$cgroup_dir/cpuset.cpus.effective")"
    printf '  CPU quota: %s\n' "$(<"$cgroup_dir/cpu.max")"
    printf '  memory.high: %s\n' "$(<"$cgroup_dir/memory.high")"
    printf '  memory.max: %s\n' "$(<"$cgroup_dir/memory.max")"
    printf '  memory.swap.max: %s\n' "$(<"$cgroup_dir/memory.swap.max")"
    printf '  CARGO_BUILD_JOBS: %s\n' "${CARGO_BUILD_JOBS:-unset}"
    printf '  release codegen units: %s\n' "${CARGO_PROFILE_RELEASE_CODEGEN_UNITS:-unset}"
    printf '  release LTO: %s\n' "${CARGO_PROFILE_RELEASE_LTO:-unset}"
}

run_linux_command() {
    local command_path="$1"
    shift

    local caller_group
    local caller_home
    local caller_uid
    local caller_user
    local unit_name
    local -a privilege_prefix=()

    if ! command -v systemd-run >/dev/null 2>&1; then
        error "resource-limited builds require systemd-run on Linux"
        return 1
    fi

    calculate_policy

    caller_user="$(id -un)"
    caller_group="$(id -gn)"
    caller_uid="$(id -u)"
    caller_home="${HOME:?HOME must be set}"
    unit_name="br-build-limit-${caller_uid}-${BASHPID}"

    if (( EUID != 0 )); then
        if ! command -v sudo >/dev/null 2>&1 || ! sudo -n true 2>/dev/null; then
            error "passwordless sudo is required to create a transient build cgroup"
            return 1
        fi
        privilege_prefix=(sudo -n)
    fi

    "${privilege_prefix[@]}" systemd-run \
        --unit="$unit_name" \
        --wait \
        --pipe \
        --collect \
        --uid="$caller_user" \
        --gid="$caller_group" \
        --working-directory="$PWD" \
        --setenv="HOME=$caller_home" \
        --setenv="PATH=$PATH" \
        --setenv="CARGO_BUILD_JOBS=$CARGO_JOBS" \
        --setenv="CARGO_PROFILE_RELEASE_CODEGEN_UNITS=$CODEGEN_UNITS" \
        --setenv="CARGO_PROFILE_RELEASE_LTO=$RELEASE_LTO" \
        --property="AllowedCPUs=$SELECTED_CPU_SET" \
        --property="CPUQuota=${CPU_QUOTA_PERCENT}%" \
        --property="MemoryHigh=$MEMORY_HIGH_BYTES" \
        --property="MemoryMax=$MEMORY_MAX_BYTES" \
        --property=MemorySwapMax=0 \
        --property=Nice=5 \
        -- "$command_path" "$@"
}

run_macos_command() {
    local command_path="$1"
    shift

    calculate_policy
    printf '%s\n' \
        'note: macOS cannot provide cgroup containment; using weaker reduced-priority isolation' >&2
    CARGO_BUILD_JOBS="$CARGO_JOBS" \
        CARGO_PROFILE_RELEASE_CODEGEN_UNITS="$CODEGEN_UNITS" \
        CARGO_PROFILE_RELEASE_LTO="$RELEASE_LTO" \
        nice -n 5 -- "$command_path" "$@"
}

run_command() {
    local requested_command="$1"
    shift

    local command_path
    command_path="$(resolve_command "$requested_command")"
    if [[ -z "$command_path" || ! -x "$command_path" ]]; then
        error "command is not executable or was not found: $requested_command"
        return 1
    fi

    case "$HOST_OS" in
        Linux) run_linux_command "$command_path" "$@" ;;
        Darwin) run_macos_command "$command_path" "$@" ;;
        *)
            error "unsupported host for resource-limited builds: $HOST_OS"
            return 1
            ;;
    esac
}

check_host_limits() {
    local script_dir
    local script_path

    if [[ "$HOST_OS" == "Darwin" ]]; then
        print_plan
        printf '%s\n' 'note: macOS commands use weaker reduced-priority isolation' >&2
        return
    fi

    script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
    script_path="$script_dir/$(basename -- "${BASH_SOURCE[0]}")"
    run_command "$script_path" --check-inside
}

usage() {
    cat <<'EOF'
Usage:
  ./scripts/with-build-limits.sh COMMAND [ARGUMENT ...]
  ./scripts/with-build-limits.sh --check
  ./scripts/with-build-limits.sh --plan

On Linux, the wrapper places the command and all descendants in a transient
cgroup. By default it selects half of the CPUs available to the process, sets a
matching CPU quota, caps memory at half of effective memory, and begins reclaim
at three quarters of that cap. Effective memory is the smaller of physical
memory and any finite inherited cgroup ceiling.

On macOS, cgroup containment is unavailable. The wrapper retains conservative
Cargo settings and runs the command at reduced priority while reporting that
the isolation is weaker. Other platforms fail without running the command.

Portable policy overrides:
  BR_BUILD_CPU_SHARE           CPU fraction in the range (0, 1]
  BR_BUILD_MEMORY_MAX_SHARE    Effective-memory fraction in the range (0, 1]
  BR_BUILD_MEMORY_HIGH_SHARE   Memory-max fraction in the range (0, 1)
  BR_BUILD_CARGO_JOBS          Positive Cargo job count (default: 2)
  BR_BUILD_CODEGEN_UNITS       Positive release codegen-unit count (default: 4)
  BR_BUILD_LTO                 false, true, thin, or fat (default: false)
EOF
}

main() {
    case "${1:-}" in
        --check)
            check_host_limits
            ;;
        --check-inside)
            if [[ "$HOST_OS" != "Linux" ]]; then
                error "cgroup inspection is only available on Linux"
                return 1
            fi
            check_inside_cgroup
            ;;
        --plan)
            print_plan
            ;;
        --help|-h)
            usage
            ;;
        "")
            usage >&2
            return 2
            ;;
        *)
            run_command "$@"
            ;;
    esac
}

main "$@"

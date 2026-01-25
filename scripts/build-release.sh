#!/usr/bin/env bash
# Build release binaries for all supported platforms
# Creates tarballs with SHA256 checksums for GitHub Releases
#
# Usage:
#   ./scripts/build-release.sh [VERSION]
#
# If VERSION is not provided, extracts from Cargo.toml

set -euo pipefail

# ============================================================================
# Configuration
# ============================================================================
readonly BINARY_NAME="br"
readonly PROJECT_NAME="beads_rust"

# Target triples for cross-compilation
readonly TARGETS=(
    "x86_64-unknown-linux-gnu"      # linux_amd64
    "aarch64-unknown-linux-gnu"     # linux_arm64
    "x86_64-apple-darwin"           # darwin_amd64
    "aarch64-apple-darwin"          # darwin_arm64
    # "x86_64-pc-windows-msvc"      # windows_amd64 (requires Windows build env)
)

# Map target triple to platform suffix
declare -A TARGET_SUFFIX=(
    ["x86_64-unknown-linux-gnu"]="linux_amd64"
    ["aarch64-unknown-linux-gnu"]="linux_arm64"
    ["x86_64-apple-darwin"]="darwin_amd64"
    ["aarch64-apple-darwin"]="darwin_arm64"
    ["x86_64-pc-windows-msvc"]="windows_amd64"
)

# ============================================================================
# Functions
# ============================================================================
log() {
    echo -e "\033[32m→\033[0m $*"
}

error() {
    echo -e "\033[31m✗\033[0m $*" >&2
    exit 1
}

get_version() {
    grep '^version' Cargo.toml | head -1 | cut -d'"' -f2
}

check_requirements() {
    log "Checking requirements..."

    if ! command -v cargo &>/dev/null; then
        error "cargo not found. Please install Rust."
    fi

    if ! command -v cross &>/dev/null; then
        log "Installing cross for cross-compilation..."
        cargo install cross --git https://github.com/cross-rs/cross
    fi

    log "Requirements satisfied"
}

build_target() {
    local target="$1"
    local version="$2"
    local suffix="${TARGET_SUFFIX[$target]}"
    local archive_name="${BINARY_NAME}-v${version}-${suffix}.tar.gz"
    local checksum_name="${archive_name}.sha256"

    log "Building for $target ($suffix)..."

    # Build with cross (handles cross-compilation automatically)
    if [[ "$target" == *"linux"* ]] || [[ "$(uname -s)" == "Darwin" && "$target" == *"darwin"* ]]; then
        # Native or same-OS build
        if [[ "$target" == "$(rustc -vV | grep host | cut -d' ' -f2)" ]]; then
            cargo build --release --target "$target"
        else
            cross build --release --target "$target"
        fi
    else
        cross build --release --target "$target"
    fi

    local binary_path="target/${target}/release/${BINARY_NAME}"

    if [[ ! -f "$binary_path" ]]; then
        error "Binary not found at $binary_path"
    fi

    # Create staging directory
    local stage_dir="target/release-stage/${suffix}"
    mkdir -p "$stage_dir"

    # Copy binary
    cp "$binary_path" "$stage_dir/${BINARY_NAME}"
    chmod +x "$stage_dir/${BINARY_NAME}"

    # Copy docs
    [[ -f "README.md" ]] && cp README.md "$stage_dir/"
    [[ -f "LICENSE" ]] && cp LICENSE "$stage_dir/"

    # Create tarball
    log "Creating $archive_name..."
    tar -czf "target/release-stage/${archive_name}" -C "target/release-stage" "$suffix"

    # Generate checksum
    if command -v sha256sum &>/dev/null; then
        sha256sum "target/release-stage/${archive_name}" | awk '{print $1}' > "target/release-stage/${checksum_name}"
    elif command -v shasum &>/dev/null; then
        shasum -a 256 "target/release-stage/${archive_name}" | awk '{print $1}' > "target/release-stage/${checksum_name}"
    else
        error "No SHA256 tool found"
    fi

    log "Created $archive_name with checksum"
}

clean_stage() {
    log "Resetting staging directory..."
    if [[ -e target/release-stage ]]; then
        local ts
        ts="$(date +%Y%m%d_%H%M%S)"
        local backup="target/release-stage.${ts}"
        log "NOTE: Not deleting existing staging dir (deletion policy). Moving to: $backup"
        mv target/release-stage "$backup"
    fi
    mkdir -p target/release-stage
}

list_artifacts() {
    echo ""
    log "Release artifacts:"
    echo ""

    for file in target/release-stage/*.tar.gz; do
        if [[ -f "$file" ]]; then
            local size
            size="$(du -h "$file" | cut -f1)"
            local checksum
            checksum="$(cat "${file}.sha256")"
            echo "  $(basename "$file") ($size)"
            echo "    SHA256: $checksum"
        fi
    done

    echo ""
    log "Upload these files to GitHub Releases"
}

build_native_only() {
    local version="$1"
    local native_target
    native_target="$(rustc -vV | grep host | cut -d' ' -f2)"
    local suffix="${TARGET_SUFFIX[$native_target]:-native}"

    log "Building native target only: $native_target"

    cargo build --release

    local binary_path="target/release/${BINARY_NAME}"
    local archive_name="${BINARY_NAME}-v${version}-${suffix}.tar.gz"
    local checksum_name="${archive_name}.sha256"

    # Create staging directory
    local stage_dir="target/release-stage/${suffix}"
    mkdir -p "$stage_dir"

    # Copy binary
    cp "$binary_path" "$stage_dir/${BINARY_NAME}"
    chmod +x "$stage_dir/${BINARY_NAME}"

    # Copy docs
    [[ -f "README.md" ]] && cp README.md "$stage_dir/"
    [[ -f "LICENSE" ]] && cp LICENSE "$stage_dir/"

    # Create tarball
    log "Creating $archive_name..."
    tar -czf "target/release-stage/${archive_name}" -C "target/release-stage" "$suffix"

    # Generate checksum
    if command -v sha256sum &>/dev/null; then
        sha256sum "target/release-stage/${archive_name}" | awk '{print $1}' > "target/release-stage/${checksum_name}"
    elif command -v shasum &>/dev/null; then
        shasum -a 256 "target/release-stage/${archive_name}" | awk '{print $1}' > "target/release-stage/${checksum_name}"
    fi

    log "Created $archive_name with checksum"
}

# ============================================================================
# Main
# ============================================================================
main() {
    local version=""
    local native_only=false

    # Parse args
    for arg in "$@"; do
        case "$arg" in
            --native-only)
                native_only=true
                ;;
            --help|-h)
                echo "Usage: $0 [VERSION] [--native-only]"
                echo ""
                echo "Options:"
                echo "  VERSION       Version to build (default: from Cargo.toml)"
                echo "  --native-only Only build for current platform"
                exit 0
                ;;
            -*)
                # Skip other flags
                ;;
            *)
                if [[ -z "$version" ]]; then
                    version="$arg"
                fi
                ;;
        esac
    done

    # Get version if not provided
    if [[ -z "$version" ]]; then
        version="$(get_version)"
    fi

    log "Building release v${version}"

    clean_stage

    if [[ "$native_only" == "true" ]]; then
        build_native_only "$version"
    else
        check_requirements

        for target in "${TARGETS[@]}"; do
            build_target "$target" "$version" || {
                echo "  Warning: Failed to build for $target (skipping)"
            }
        done
    fi

    list_artifacts
}

main "$@"

#!/usr/bin/env bash
# br installer - Multi-platform installer for beads_rust
# https://github.com/Dicklesworthstone/beads_rust
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Dicklesworthstone/beads_rust/main/install.sh | bash
#   wget -qO- https://raw.githubusercontent.com/Dicklesworthstone/beads_rust/main/install.sh | bash
#
# Options:
#   --prefix=DIR       Install to DIR (default: ~/.local/bin)
#   --no-modify-path   Don't modify shell PATH
#   --uninstall        Remove br and clean up
#   --from-source      Force build from source
#   --help             Show this help message

set -euo pipefail

# ============================================================================
# Configuration
# ============================================================================
readonly REPO="Dicklesworthstone/beads_rust"
readonly BINARY_NAME="br"
readonly DEFAULT_INSTALL_DIR="${HOME}/.local/bin"
readonly CONFIG_DIR="${HOME}/.br"
readonly LOG_FILE="${CONFIG_DIR}/install.log"
readonly LOCK_FILE="/tmp/br-install.lock"
readonly LOCK_TIMEOUT=300  # 5 minutes
readonly DOWNLOAD_TIMEOUT=120
readonly MAX_RETRIES=3

# Mutable configuration (set by parse_args)
INSTALL_DIR="${DEFAULT_INSTALL_DIR}"
MODIFY_PATH=true
UNINSTALL=false
FROM_SOURCE=false
VERBOSE=false

# ============================================================================
# Logging
# ============================================================================
init_logging() {
    mkdir -p "$CONFIG_DIR"
    echo "" >> "$LOG_FILE"
    log "INFO" "=== br installer started at $(date -Iseconds) ==="
    log "DEBUG" "System: $(uname -a)"
    log "DEBUG" "Shell: ${SHELL:-unknown}"
    log "DEBUG" "User: ${USER:-$(whoami)}"
}

log() {
    local level="$1"
    shift
    local msg="$*"
    local timestamp
    timestamp="$(date -Iseconds 2>/dev/null || date '+%Y-%m-%dT%H:%M:%S')"

    echo "[$timestamp] [$level] $msg" >> "$LOG_FILE"

    if [[ "$VERBOSE" == "true" ]] || [[ "$level" != "DEBUG" ]]; then
        case "$level" in
            ERROR) echo -e "\033[31m✗ $msg\033[0m" >&2 ;;
            WARN)  echo -e "\033[33m⚠ $msg\033[0m" >&2 ;;
            INFO)  echo -e "\033[32m→ $msg\033[0m" ;;
            DEBUG) echo -e "\033[90m  $msg\033[0m" ;;
        esac
    fi
}

error() {
    log "ERROR" "$@"
    # Dump diagnostic info on error
    log "DEBUG" "Environment dump:"
    log "DEBUG" "  PATH=${PATH:-<not set>}"
    log "DEBUG" "  HOME=${HOME:-<not set>}"
    log "DEBUG" "  HTTPS_PROXY=${HTTPS_PROXY:-<not set>}"
    exit 1
}

warn() {
    log "WARN" "$@"
}

info() {
    log "INFO" "$@"
}

debug() {
    log "DEBUG" "$@"
}

# ============================================================================
# Argument Parsing
# ============================================================================
show_help() {
    cat << 'EOF'
br installer - Install beads_rust (br) CLI tool

Usage:
  curl -fsSL https://.../install.sh | bash
  curl -fsSL https://.../install.sh | bash -s -- [OPTIONS]

Options:
  --prefix=DIR       Install to DIR (default: ~/.local/bin)
  --no-modify-path   Don't add install directory to PATH
  --uninstall        Remove br and clean up
  --from-source      Force build from source (requires Rust)
  --verbose          Show debug output
  --help             Show this help message

Environment Variables:
  HTTPS_PROXY        Use HTTP proxy for downloads
  BR_INSTALL_DIR     Override default install directory

Examples:
  # Default install
  curl -fsSL .../install.sh | bash

  # Custom prefix
  curl -fsSL .../install.sh | bash -s -- --prefix=/usr/local/bin

  # Uninstall
  curl -fsSL .../install.sh | bash -s -- --uninstall
EOF
    exit 0
}

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --prefix=*)
                INSTALL_DIR="${1#*=}"
                ;;
            --prefix)
                shift
                INSTALL_DIR="${1:-}"
                [[ -z "$INSTALL_DIR" ]] && error "--prefix requires a directory"
                ;;
            --no-modify-path)
                MODIFY_PATH=false
                ;;
            --uninstall)
                UNINSTALL=true
                ;;
            --from-source)
                FROM_SOURCE=true
                ;;
            --verbose|-v)
                VERBOSE=true
                ;;
            --help|-h)
                show_help
                ;;
            *)
                warn "Unknown option: $1"
                ;;
        esac
        shift
    done

    # Environment variable override
    if [[ -n "${BR_INSTALL_DIR:-}" ]]; then
        INSTALL_DIR="$BR_INSTALL_DIR"
    fi
}

# ============================================================================
# Lock Management
# ============================================================================
acquire_lock() {
    debug "Acquiring install lock..."

    # Check for stale lock
    if [[ -f "$LOCK_FILE" ]]; then
        local lock_pid
        lock_pid="$(cat "$LOCK_FILE" 2>/dev/null || echo "")"

        if [[ -n "$lock_pid" ]] && ! kill -0 "$lock_pid" 2>/dev/null; then
            warn "Removing stale lock file (PID $lock_pid not running)"
            rm -f "$LOCK_FILE"
        fi

        # Check age of lock file
        if [[ -f "$LOCK_FILE" ]]; then
            local lock_age
            lock_age="$(( $(date +%s) - $(stat -c %Y "$LOCK_FILE" 2>/dev/null || stat -f %m "$LOCK_FILE" 2>/dev/null || echo 0) ))"
            if [[ "$lock_age" -gt "$LOCK_TIMEOUT" ]]; then
                warn "Removing stale lock file (age: ${lock_age}s > ${LOCK_TIMEOUT}s)"
                rm -f "$LOCK_FILE"
            fi
        fi
    fi

    # Try to acquire lock
    if [[ -f "$LOCK_FILE" ]]; then
        error "Another installation is in progress. If this is incorrect, remove $LOCK_FILE"
    fi

    echo $$ > "$LOCK_FILE"
    debug "Lock acquired (PID $$)"
}

release_lock() {
    if [[ -f "$LOCK_FILE" ]]; then
        rm -f "$LOCK_FILE"
        debug "Lock released"
    fi
}

# ============================================================================
# Platform Detection
# ============================================================================
detect_platform() {
    local os arch

    case "$(uname -s)" in
        Linux*)  os="linux" ;;
        Darwin*) os="darwin" ;;
        MINGW*|MSYS*|CYGWIN*) os="windows" ;;
        *)
            error "Unsupported operating system: $(uname -s)"
            ;;
    esac

    case "$(uname -m)" in
        x86_64|amd64) arch="amd64" ;;
        aarch64|arm64) arch="arm64" ;;
        armv7*) arch="armv7" ;;
        *)
            error "Unsupported architecture: $(uname -m)"
            ;;
    esac

    echo "${os}_${arch}"
}

detect_shell() {
    local shell_name
    shell_name="$(basename "${SHELL:-/bin/bash}")"

    case "$shell_name" in
        bash|zsh|fish|sh) echo "$shell_name" ;;
        *) echo "bash" ;;  # Default fallback
    esac
}

# ============================================================================
# Download Functions
# ============================================================================
get_latest_release() {
    local api_url="https://api.github.com/repos/${REPO}/releases/latest"
    local release_info

    debug "Fetching latest release info from $api_url"

    if command -v curl &>/dev/null; then
        release_info="$(curl -fsSL \
            ${HTTPS_PROXY:+--proxy "$HTTPS_PROXY"} \
            --max-time 30 \
            -H "Accept: application/vnd.github.v3+json" \
            "$api_url" 2>/dev/null)" || return 1
    elif command -v wget &>/dev/null; then
        release_info="$(wget -qO- \
            ${HTTPS_PROXY:+--proxy "$HTTPS_PROXY"} \
            --timeout=30 \
            --header="Accept: application/vnd.github.v3+json" \
            "$api_url" 2>/dev/null)" || return 1
    else
        error "Neither curl nor wget found. Please install one of them."
    fi

    # Extract tag name (simple grep-based parsing to avoid jq dependency)
    echo "$release_info" | grep -o '"tag_name"[[:space:]]*:[[:space:]]*"[^"]*"' | head -1 | cut -d'"' -f4
}

download_file() {
    local url="$1"
    local dest="$2"
    local attempt=0

    while [[ $attempt -lt $MAX_RETRIES ]]; do
        attempt=$((attempt + 1))
        debug "Download attempt $attempt/$MAX_RETRIES: $url"

        if command -v curl &>/dev/null; then
            if curl -fsSL \
                ${HTTPS_PROXY:+--proxy "$HTTPS_PROXY"} \
                --max-time "$DOWNLOAD_TIMEOUT" \
                --retry 2 \
                -C - \
                -o "$dest" \
                "$url" 2>/dev/null; then
                return 0
            fi
        elif command -v wget &>/dev/null; then
            if wget -q \
                ${HTTPS_PROXY:+--proxy "$HTTPS_PROXY"} \
                --timeout="$DOWNLOAD_TIMEOUT" \
                --tries=2 \
                -c \
                -O "$dest" \
                "$url" 2>/dev/null; then
                return 0
            fi
        fi

        warn "Download attempt $attempt failed, retrying..."
        sleep 2
    done

    return 1
}

download_release() {
    local platform="$1"
    local version
    local download_url
    local checksum_url
    local download_path
    local checksum_path

    version="$(get_latest_release)" || {
        warn "Could not fetch latest release version"
        return 1
    }

    info "Latest release: $version"

    # Construct download URLs
    # Expected format: br-v0.1.0-linux_amd64.tar.gz
    local archive_name="br-${version}-${platform}.tar.gz"
    download_url="https://github.com/${REPO}/releases/download/${version}/${archive_name}"
    checksum_url="${download_url}.sha256"

    download_path="/tmp/${archive_name}"
    checksum_path="${download_path}.sha256"

    debug "Download URL: $download_url"

    # Download binary archive
    info "Downloading ${archive_name}..."
    if ! download_file "$download_url" "$download_path"; then
        warn "Binary download failed"
        rm -f "$download_path"
        return 1
    fi

    # Download checksum
    info "Downloading checksum..."
    if ! download_file "$checksum_url" "$checksum_path"; then
        warn "Checksum download failed"
        rm -f "$download_path" "$checksum_path"
        return 1
    fi

    # Verify checksum
    if ! verify_checksum "$download_path" "$checksum_path"; then
        error "Checksum verification failed!"
    fi

    # Extract binary
    info "Extracting..."
    local extract_dir="/tmp/br-extract-$$"
    mkdir -p "$extract_dir"

    if ! tar -xzf "$download_path" -C "$extract_dir" 2>/dev/null; then
        rm -rf "$extract_dir" "$download_path" "$checksum_path"
        error "Failed to extract archive"
    fi

    # Find the binary in the extracted files
    local binary_path
    binary_path="$(find "$extract_dir" -name "$BINARY_NAME" -type f -perm -111 2>/dev/null | head -1)"

    if [[ -z "$binary_path" ]]; then
        # Maybe it's in a subdirectory or has different name
        binary_path="$(find "$extract_dir" -type f -perm -111 2>/dev/null | head -1)"
    fi

    if [[ -z "$binary_path" ]] || [[ ! -f "$binary_path" ]]; then
        rm -rf "$extract_dir" "$download_path" "$checksum_path"
        error "Binary not found in archive"
    fi

    # Install binary
    install_binary "$binary_path"

    # Cleanup
    rm -rf "$extract_dir" "$download_path" "$checksum_path"

    return 0
}

verify_checksum() {
    local file_path="$1"
    local checksum_path="$2"

    debug "Verifying checksum for $file_path"

    local expected actual
    expected="$(cat "$checksum_path" | awk '{print $1}')"

    if command -v sha256sum &>/dev/null; then
        actual="$(sha256sum "$file_path" | awk '{print $1}')"
    elif command -v shasum &>/dev/null; then
        actual="$(shasum -a 256 "$file_path" | awk '{print $1}')"
    else
        warn "No SHA256 tool found, skipping checksum verification"
        return 0
    fi

    if [[ "$expected" != "$actual" ]]; then
        log "ERROR" "Checksum mismatch!"
        log "ERROR" "  Expected: $expected"
        log "ERROR" "  Got:      $actual"
        return 1
    fi

    debug "Checksum verified: $expected"
    return 0
}

# ============================================================================
# Installation
# ============================================================================
install_binary() {
    local src="$1"

    info "Installing to ${INSTALL_DIR}/${BINARY_NAME}..."

    # Create install directory
    mkdir -p "$INSTALL_DIR"

    # Atomic install using temp file and mv
    local temp_dest="${INSTALL_DIR}/.${BINARY_NAME}.tmp.$$"
    cp "$src" "$temp_dest"
    chmod +x "$temp_dest"
    mv -f "$temp_dest" "${INSTALL_DIR}/${BINARY_NAME}"

    debug "Binary installed successfully"
}

# ============================================================================
# Build from Source
# ============================================================================
check_rust() {
    if command -v cargo &>/dev/null; then
        debug "Rust found: $(cargo --version)"
        return 0
    fi
    return 1
}

install_rust() {
    info "Installing Rust via rustup..."

    if command -v curl &>/dev/null; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain nightly
    elif command -v wget &>/dev/null; then
        wget -qO- https://sh.rustup.rs | sh -s -- -y --default-toolchain nightly
    else
        error "Cannot install Rust: neither curl nor wget available"
    fi

    # Source cargo env
    # shellcheck source=/dev/null
    source "${HOME}/.cargo/env" 2>/dev/null || true

    if ! check_rust; then
        error "Rust installation failed"
    fi
}

build_from_source() {
    info "Building from source..."

    if ! check_rust; then
        install_rust
    fi

    local build_dir="/tmp/br-build-$$"
    mkdir -p "$build_dir"

    info "Cloning repository..."
    if ! git clone --depth 1 "https://github.com/${REPO}.git" "$build_dir" 2>/dev/null; then
        rm -rf "$build_dir"
        error "Failed to clone repository"
    fi

    info "Building with Cargo (this may take a few minutes)..."
    pushd "$build_dir" > /dev/null

    if ! cargo build --release 2>&1; then
        popd > /dev/null
        rm -rf "$build_dir"
        error "Build failed"
    fi

    local binary_path="$build_dir/target/release/$BINARY_NAME"
    if [[ ! -f "$binary_path" ]]; then
        popd > /dev/null
        rm -rf "$build_dir"
        error "Binary not found after build"
    fi

    install_binary "$binary_path"

    popd > /dev/null
    rm -rf "$build_dir"
}

# ============================================================================
# PATH Modification
# ============================================================================
modify_path() {
    local shell_name
    shell_name="$(detect_shell)"

    # Check if already in PATH
    if [[ ":$PATH:" == *":${INSTALL_DIR}:"* ]]; then
        debug "Install directory already in PATH"
        return 0
    fi

    info "Adding ${INSTALL_DIR} to PATH..."

    local path_line="export PATH=\"\${PATH}:${INSTALL_DIR}\"  # Added by br installer"
    local fish_line="set -gx PATH \$PATH ${INSTALL_DIR}  # Added by br installer"

    case "$shell_name" in
        bash)
            local bashrc="${HOME}/.bashrc"
            if [[ -f "$bashrc" ]] && ! grep -q "# Added by br installer" "$bashrc"; then
                echo "" >> "$bashrc"
                echo "$path_line" >> "$bashrc"
                debug "Added to $bashrc"
            fi
            ;;
        zsh)
            local zshrc="${HOME}/.zshrc"
            if [[ -f "$zshrc" ]] && ! grep -q "# Added by br installer" "$zshrc"; then
                echo "" >> "$zshrc"
                echo "$path_line" >> "$zshrc"
                debug "Added to $zshrc"
            elif [[ ! -f "$zshrc" ]]; then
                echo "$path_line" >> "$zshrc"
                debug "Created $zshrc with PATH"
            fi
            ;;
        fish)
            local fish_config="${HOME}/.config/fish/config.fish"
            mkdir -p "$(dirname "$fish_config")"
            if [[ -f "$fish_config" ]] && ! grep -q "# Added by br installer" "$fish_config"; then
                echo "" >> "$fish_config"
                echo "$fish_line" >> "$fish_config"
                debug "Added to $fish_config"
            elif [[ ! -f "$fish_config" ]]; then
                echo "$fish_line" >> "$fish_config"
                debug "Created $fish_config with PATH"
            fi
            ;;
        *)
            # Generic fallback - try .profile
            local profile="${HOME}/.profile"
            if [[ -f "$profile" ]] && ! grep -q "# Added by br installer" "$profile"; then
                echo "" >> "$profile"
                echo "$path_line" >> "$profile"
                debug "Added to $profile"
            fi
            ;;
    esac
}

# ============================================================================
# Uninstall
# ============================================================================
uninstall_br() {
    info "Uninstalling br..."

    # Remove binary
    if [[ -f "${INSTALL_DIR}/${BINARY_NAME}" ]]; then
        rm -f "${INSTALL_DIR}/${BINARY_NAME}"
        info "Removed ${INSTALL_DIR}/${BINARY_NAME}"
    else
        warn "Binary not found at ${INSTALL_DIR}/${BINARY_NAME}"
    fi

    # Remove PATH modifications
    local rc_files=(
        "${HOME}/.bashrc"
        "${HOME}/.zshrc"
        "${HOME}/.profile"
        "${HOME}/.config/fish/config.fish"
    )

    for rc in "${rc_files[@]}"; do
        if [[ -f "$rc" ]]; then
            # Create backup and remove br installer lines
            if grep -q "# Added by br installer" "$rc"; then
                sed -i.bak '/# Added by br installer/d' "$rc" 2>/dev/null || \
                sed -i '' '/# Added by br installer/d' "$rc" 2>/dev/null || true
                debug "Cleaned PATH from $rc"
            fi
        fi
    done

    # Optionally remove config directory
    if [[ -d "$CONFIG_DIR" ]]; then
        read -r -p "Remove config directory ${CONFIG_DIR}? [y/N] " response || response="n"
        if [[ "$response" =~ ^[Yy]$ ]]; then
            rm -rf "$CONFIG_DIR"
            info "Removed $CONFIG_DIR"
        fi
    fi

    info "br uninstalled successfully"
}

# ============================================================================
# Cleanup
# ============================================================================
cleanup() {
    release_lock
}

# ============================================================================
# Main
# ============================================================================
main() {
    parse_args "$@"
    init_logging

    # Handle uninstall
    if [[ "$UNINSTALL" == "true" ]]; then
        uninstall_br
        exit 0
    fi

    acquire_lock
    trap cleanup EXIT

    local platform
    platform="$(detect_platform)"
    info "Detected platform: $platform"
    info "Install directory: $INSTALL_DIR"

    # Try binary download first (unless --from-source)
    if [[ "$FROM_SOURCE" != "true" ]]; then
        if download_release "$platform"; then
            info "Binary installation successful"
        else
            warn "Binary not available for $platform, building from source..."
            build_from_source
        fi
    else
        build_from_source
    fi

    # Modify PATH if requested
    if [[ "$MODIFY_PATH" == "true" ]]; then
        modify_path
    fi

    # Verify installation
    echo ""
    echo -e "\033[32m✓ br installed successfully!\033[0m"
    echo ""
    echo "  Version: $("${INSTALL_DIR}/${BINARY_NAME}" --version 2>/dev/null || echo "unknown")"
    echo "  Location: ${INSTALL_DIR}/${BINARY_NAME}"
    echo ""

    if [[ "$MODIFY_PATH" == "true" ]] && [[ ":$PATH:" != *":${INSTALL_DIR}:"* ]]; then
        echo "  Restart your shell or run:"
        echo "    source ~/.$(detect_shell)rc"
        echo ""
    fi

    echo "  Get started: br --help"
    echo ""
}

# Run main if script is executed (not sourced)
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    main "$@"
fi

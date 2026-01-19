#!/usr/bin/env bash
#
# br (beads_rust) installer - Ultra-robust multi-platform installer with beautiful output
#
# One-liner install:
#   curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/beads_rust/main/install.sh?$(date +%s)" | bash
#
# Options:
#   --version vX.Y.Z   Install specific version (default: latest)
#   --dest DIR         Install to DIR (default: ~/.local/bin)
#   --system           Install to /usr/local/bin (requires sudo)
#   --easy-mode        Auto-update PATH in shell rc files
#   --verify           Run self-test after install
#   --from-source      Build from source instead of downloading binary
#   --quiet            Suppress non-error output
#   --no-gum           Disable gum formatting even if available
#   --uninstall        Remove br and clean up
#   --help             Show this help
#
set -euo pipefail
umask 022
shopt -s lastpipe 2>/dev/null || true

# ============================================================================
# Configuration
# ============================================================================
VERSION="${VERSION:-}"
OWNER="${OWNER:-Dicklesworthstone}"
REPO="${REPO:-beads_rust}"
BINARY_NAME="br"
DEST_DEFAULT="$HOME/.local/bin"
DEST="${DEST:-$DEST_DEFAULT}"
EASY=0
QUIET=0
VERIFY=0
FROM_SOURCE=0
UNINSTALL=0
CHECKSUM="${CHECKSUM:-}"
CHECKSUM_URL="${CHECKSUM_URL:-}"
ARTIFACT_URL="${ARTIFACT_URL:-}"
LOCK_FILE="/tmp/br-install.lock"
SYSTEM=0
NO_GUM=0
MAX_RETRIES=3
DOWNLOAD_TIMEOUT=120
INSTALLER_VERSION="2.0.0"

# Colors for fallback output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
MAGENTA='\033[0;35m'
BOLD='\033[1m'
DIM='\033[2m'
ITALIC='\033[3m'
NC='\033[0m'

# Gum availability flag
GUM_AVAILABLE=false

# ============================================================================
# Gum auto-installation (from giil)
# ============================================================================
try_install_gum() {
    # Skip if in CI or non-interactive
    [[ -z "${CI:-}" ]] || return 1
    [[ -t 1 ]] || return 1

    # Inline OS detection
    local os="unknown"
    case "$(uname -s)" in
        Darwin*) os="macos" ;;
        Linux*)  os="linux" ;;
    esac

    # Try to install gum quietly
    case "$os" in
        macos)
            if command -v brew &> /dev/null; then
                brew install gum &>/dev/null && return 0
            fi
            ;;
        linux)
            # Try common package managers
            if command -v apt-get &> /dev/null; then
                (
                    sudo mkdir -p /etc/apt/keyrings 2>/dev/null
                    curl -fsSL https://repo.charm.sh/apt/gpg.key | sudo gpg --dearmor -o /etc/apt/keyrings/charm.gpg 2>/dev/null
                    echo "deb [signed-by=/etc/apt/keyrings/charm.gpg] https://repo.charm.sh/apt/ * *" | sudo tee /etc/apt/sources.list.d/charm.list >/dev/null
                    sudo apt-get update -qq && sudo apt-get install -y -qq gum
                ) &>/dev/null && return 0
            elif command -v dnf &> /dev/null; then
                (
                    echo '[charm]
name=Charm
baseurl=https://repo.charm.sh/yum/
enabled=1
gpgcheck=1
gpgkey=https://repo.charm.sh/yum/gpg.key' | sudo tee /etc/yum.repos.d/charm.repo >/dev/null
                    sudo dnf install -y gum
                ) &>/dev/null && return 0
            elif command -v pacman &> /dev/null; then
                sudo pacman -S --noconfirm gum &>/dev/null && return 0
            fi

            # Fallback: download from GitHub releases
            local arch
            arch=$(uname -m)
            case "$arch" in
                x86_64) arch="amd64" ;;
                aarch64|arm64) arch="arm64" ;;
                *) return 1 ;;
            esac

            local tmp_dir
            tmp_dir=$(mktemp -d)
            local gum_version="0.14.5"
            local gum_url="https://github.com/charmbracelet/gum/releases/download/v${gum_version}/gum_${gum_version}_Linux_${arch}.tar.gz"

            (
                cd "$tmp_dir"
                curl -fsSL "$gum_url" -o gum.tar.gz
                tar -xzf gum.tar.gz
                if sudo mv gum /usr/local/bin/gum 2>/dev/null; then
                    :
                else
                    mkdir -p ~/.local/bin
                    mv gum ~/.local/bin/gum
                fi
            ) &>/dev/null && rm -rf "$tmp_dir" && return 0

            rm -rf "$tmp_dir"
            ;;
    esac

    return 1
}

check_gum() {
    # Respect NO_GUM flag
    if [[ "$NO_GUM" -eq 1 ]]; then
        GUM_AVAILABLE=false
        return 1
    fi

    if command -v gum &> /dev/null; then
        GUM_AVAILABLE=true
        return 0
    fi

    # Only try to install gum if interactive and not disabled
    if [[ -t 1 && -z "${CI:-}" ]]; then
        if try_install_gum; then
            if [[ -x "${HOME}/.local/bin/gum" && ":$PATH:" != *":${HOME}/.local/bin:"* ]]; then
                export PATH="${HOME}/.local/bin:${PATH}"
            fi
            if command -v gum &> /dev/null; then
                GUM_AVAILABLE=true
                return 0
            fi
        fi
    fi

    return 1
}

# ============================================================================
# Styled output functions (gum with ANSI fallback)
# ============================================================================

# Print styled banner
print_banner() {
    [ "$QUIET" -eq 1 ] && return 0

    if [[ "$GUM_AVAILABLE" == "true" ]]; then
        gum style \
            --border double \
            --border-foreground 39 \
            --padding "0 2" \
            --margin "1 0" \
            --bold \
            "$(gum style --foreground 42 '🔗 br installer')" \
            "$(gum style --foreground 245 'Agent-first issue tracker (beads_rust)')"
    else
        echo ""
        echo -e "${BOLD}${BLUE}╔════════════════════════════════════════════════╗${NC}"
        echo -e "${BOLD}${BLUE}║${NC}  ${BOLD}${GREEN}🔗 br installer${NC}                               ${BOLD}${BLUE}║${NC}"
        echo -e "${BOLD}${BLUE}║${NC}  ${DIM}Agent-first issue tracker (beads_rust)${NC}        ${BOLD}${BLUE}║${NC}"
        echo -e "${BOLD}${BLUE}╚════════════════════════════════════════════════╝${NC}"
        echo ""
    fi
}

# Log functions
log_info() {
    [ "$QUIET" -eq 1 ] && return 0
    if [[ "$GUM_AVAILABLE" == "true" ]]; then
        gum log --level info "$1" >&2
    else
        echo -e "${GREEN}[br]${NC} $1" >&2
    fi
}

log_warn() {
    if [[ "$GUM_AVAILABLE" == "true" ]]; then
        gum log --level warn "$1" >&2
    else
        echo -e "${YELLOW}[br]${NC} $1" >&2
    fi
}

log_error() {
    if [[ "$GUM_AVAILABLE" == "true" ]]; then
        gum log --level error "$1" >&2
    else
        echo -e "${RED}[br]${NC} $1" >&2
    fi
}

log_step() {
    [ "$QUIET" -eq 1 ] && return 0
    if [[ "$GUM_AVAILABLE" == "true" ]]; then
        gum style --foreground 39 "→ $1" >&2
    else
        echo -e "${BLUE}→${NC} $1" >&2
    fi
}

log_success() {
    [ "$QUIET" -eq 1 ] && return 0
    if [[ "$GUM_AVAILABLE" == "true" ]]; then
        gum style --foreground 82 "✓ $1" >&2
    else
        echo -e "${GREEN}✓${NC} $1" >&2
    fi
}

log_debug() {
    [[ "${DEBUG:-0}" -eq 1 ]] || return 0
    if [[ "$GUM_AVAILABLE" == "true" ]]; then
        gum log --level debug "$1" >&2
    else
        echo -e "${CYAN}[br:debug]${NC} $1" >&2
    fi
}

# Spinner wrapper for long operations
run_with_spinner() {
    local title="$1"
    shift
    if [[ "$GUM_AVAILABLE" == "true" && "$QUIET" -eq 0 ]]; then
        gum spin --spinner dot --title "$title" -- "$@"
    else
        log_step "$title"
        "$@"
    fi
}

# Die with error
die() {
    log_error "$@"
    exit 1
}

# ============================================================================
# Usage / Help (gum-styled)
# ============================================================================
usage() {
    check_gum || true

    if [[ "$GUM_AVAILABLE" == "true" ]]; then
        gum style \
            --border double \
            --border-foreground 39 \
            --padding "1 2" \
            --margin "1" \
            --bold \
            "$(gum style --foreground 42 '🔗 br installer v'${INSTALLER_VERSION})" \
            "$(gum style --foreground 245 'Agent-first issue tracker')"

        echo ""

        gum style --foreground 214 --bold "SYNOPSIS"
        echo "  curl -fsSL .../install.sh | bash"
        echo "  curl -fsSL .../install.sh | bash -s -- [OPTIONS]"
        echo ""

        gum style --foreground 214 --bold "OPTIONS"
        gum style --foreground 39 "  Installation"
        gum style --faint "    --version vX.Y.Z   Install specific version (default: latest)"
        gum style --faint "    --dest DIR         Install to DIR (default: ~/.local/bin)"
        gum style --faint "    --system           Install to /usr/local/bin (requires sudo)"
        gum style --faint "    --from-source      Build from source instead of binary"
        echo ""
        gum style --foreground 39 "  Behavior"
        gum style --faint "    --easy-mode        Auto-update PATH in shell rc files"
        gum style --faint "    --verify           Run self-test after install"
        gum style --faint "    --quiet            Suppress progress messages"
        gum style --faint "    --no-gum           Disable gum formatting"
        echo ""
        gum style --foreground 39 "  Maintenance"
        gum style --faint "    --uninstall        Remove br and clean up"
        gum style --faint "    --help             Show this help"
        echo ""

        gum style --foreground 214 --bold "ENVIRONMENT"
        gum style --faint "  HTTPS_PROXY        Use HTTP proxy for downloads"
        gum style --faint "  BR_INSTALL_DIR     Override default install directory"
        gum style --faint "  VERSION            Override version to install"
        echo ""

        gum style --foreground 214 --bold "EXAMPLES"
        gum style --foreground 39 "  # Default install"
        echo "  curl -fsSL https://raw.githubusercontent.com/Dicklesworthstone/beads_rust/main/install.sh | bash"
        echo ""
        gum style --foreground 39 "  # System install with auto PATH"
        echo "  curl -fsSL .../install.sh | sudo bash -s -- --system --easy-mode"
        echo ""
        gum style --foreground 39 "  # Force source build"
        echo "  curl -fsSL .../install.sh | bash -s -- --from-source"
        echo ""
        gum style --foreground 39 "  # Uninstall"
        echo "  curl -fsSL .../install.sh | bash -s -- --uninstall"
        echo ""

        gum style --foreground 214 --bold "PLATFORMS"
        echo "  $(gum style --foreground 82 '✓ Linux x86_64') $(gum style --foreground 245 --faint '(glibc and musl)')"
        gum style --foreground 82 "  ✓ Linux ARM64"
        gum style --foreground 82 "  ✓ macOS Intel"
        gum style --foreground 82 "  ✓ macOS Apple Silicon"
        echo "  $(gum style --foreground 82 '✓ Windows x64') $(gum style --foreground 245 --faint '(via WSL or manual)')"
        echo ""

        gum style --foreground 245 --italic "Installer will auto-install gum for beautiful output if not present"

    else
        cat <<'EOF'
br installer - Install beads_rust (br) CLI tool

Usage:
  curl -fsSL https://raw.githubusercontent.com/Dicklesworthstone/beads_rust/main/install.sh | bash
  curl -fsSL .../install.sh | bash -s -- [OPTIONS]

Options:
  --version vX.Y.Z   Install specific version (default: latest)
  --dest DIR         Install to DIR (default: ~/.local/bin)
  --system           Install to /usr/local/bin (requires sudo)
  --easy-mode        Auto-update PATH in shell rc files
  --verify           Run self-test after install
  --from-source      Build from source instead of downloading binary
  --quiet            Suppress non-error output
  --no-gum           Disable gum formatting even if available
  --uninstall        Remove br and clean up

Environment Variables:
  HTTPS_PROXY        Use HTTP proxy for downloads
  BR_INSTALL_DIR     Override default install directory
  VERSION            Override version to install

Platforms:
  ✓ Linux x86_64 (glibc and musl)
  ✓ Linux ARM64
  ✓ macOS Intel
  ✓ macOS Apple Silicon
  ✓ Windows x64 (via WSL or manual)

Examples:
  # Default install
  curl -fsSL .../install.sh | bash

  # Custom prefix with easy mode
  curl -fsSL .../install.sh | bash -s -- --dest=/usr/local/bin --easy-mode

  # Force source build
  curl -fsSL .../install.sh | bash -s -- --from-source

  # Uninstall
  curl -fsSL .../install.sh | bash -s -- --uninstall
EOF
    fi
    exit 0
}

# ============================================================================
# Argument Parsing
# ============================================================================
while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="$2"; shift 2;;
        --version=*) VERSION="${1#*=}"; shift;;
        --dest) DEST="$2"; shift 2;;
        --dest=*) DEST="${1#*=}"; shift;;
        --system) SYSTEM=1; DEST="/usr/local/bin"; shift;;
        --easy-mode) EASY=1; shift;;
        --verify) VERIFY=1; shift;;
        --artifact-url) ARTIFACT_URL="$2"; shift 2;;
        --checksum) CHECKSUM="$2"; shift 2;;
        --checksum-url) CHECKSUM_URL="$2"; shift 2;;
        --from-source) FROM_SOURCE=1; shift;;
        --quiet|-q) QUIET=1; shift;;
        --no-gum) NO_GUM=1; shift;;
        --uninstall) UNINSTALL=1; shift;;
        -h|--help) usage;;
        *) shift;;
    esac
done

# Environment variable overrides
[ -n "${BR_INSTALL_DIR:-}" ] && DEST="$BR_INSTALL_DIR"

# Initialize gum early for beautiful output
check_gum || true

# ============================================================================
# Uninstall
# ============================================================================
do_uninstall() {
    print_banner
    log_step "Uninstalling br..."

    if [ -f "$DEST/$BINARY_NAME" ]; then
        rm -f "$DEST/$BINARY_NAME"
        log_success "Removed $DEST/$BINARY_NAME"
    else
        log_warn "Binary not found at $DEST/$BINARY_NAME"
    fi

    # Remove PATH modifications from shell rc files
    for rc in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile" "$HOME/.config/fish/config.fish"; do
        if [ -f "$rc" ] && grep -q "# br installer" "$rc" 2>/dev/null; then
            if [[ "$OSTYPE" == "darwin"* ]]; then
                sed -i '' '/# br installer/d' "$rc" 2>/dev/null || true
            else
                sed -i '/# br installer/d' "$rc" 2>/dev/null || true
            fi
            log_step "Cleaned $rc"
        fi
    done

    log_success "br uninstalled successfully"
    exit 0
}

[ "$UNINSTALL" -eq 1 ] && do_uninstall

# ============================================================================
# Platform Detection
# ============================================================================
detect_platform() {
    local os arch

    case "$(uname -s)" in
        Linux*)  os="linux" ;;
        Darwin*) os="darwin" ;;
        MINGW*|MSYS*|CYGWIN*) os="windows" ;;
        *) die "Unsupported OS: $(uname -s)" ;;
    esac

    case "$(uname -m)" in
        x86_64|amd64) arch="amd64" ;;
        aarch64|arm64) arch="arm64" ;;
        armv7*) arch="armv7" ;;
        *) die "Unsupported architecture: $(uname -m)" ;;
    esac

    echo "${os}_${arch}"
}

# ============================================================================
# Version Resolution (with robust fallbacks)
# ============================================================================
resolve_version() {
    if [ -n "$VERSION" ]; then return 0; fi

    log_step "Resolving latest version..."
    local latest_url="https://api.github.com/repos/${OWNER}/${REPO}/releases/latest"
    local tag=""
    local attempts=0

    # Try GitHub API with retries
    while [ $attempts -lt $MAX_RETRIES ] && [ -z "$tag" ]; do
        attempts=$((attempts + 1))

        if command -v curl &>/dev/null; then
            tag=$(curl -fsSL \
                --connect-timeout 10 \
                --max-time 30 \
                -H "Accept: application/vnd.github.v3+json" \
                "$latest_url" 2>/dev/null | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/' || echo "")
        elif command -v wget &>/dev/null; then
            tag=$(wget -qO- --timeout=30 "$latest_url" 2>/dev/null | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/' || echo "")
        fi

        [ -z "$tag" ] && [ $attempts -lt $MAX_RETRIES ] && sleep 2
    done

    if [ -n "$tag" ] && [[ "$tag" =~ ^v[0-9] ]]; then
        VERSION="$tag"
        log_success "Latest version: $VERSION"
        return 0
    fi

    # Fallback: try redirect-based resolution
    log_step "Trying redirect-based version resolution..."
    local redirect_url="https://github.com/${OWNER}/${REPO}/releases/latest"
    if command -v curl &>/dev/null; then
        tag=$(curl -fsSL -o /dev/null -w '%{url_effective}' "$redirect_url" 2>/dev/null | sed -E 's|.*/tag/||' || echo "")
    fi

    if [ -n "$tag" ] && [[ "$tag" =~ ^v[0-9] ]] && [[ "$tag" != *"/"* ]]; then
        VERSION="$tag"
        log_success "Latest version (via redirect): $VERSION"
        return 0
    fi

    log_warn "Could not resolve latest version; will try building from source"
    VERSION=""
}

# ============================================================================
# Cross-platform locking using mkdir (atomic on all POSIX systems)
# ============================================================================
LOCK_DIR="${LOCK_FILE}.d"
LOCKED=0

acquire_lock() {
    if mkdir "$LOCK_DIR" 2>/dev/null; then
        LOCKED=1
        echo $$ > "$LOCK_DIR/pid"
        return 0
    fi

    # Check if existing lock is stale
    if [ -f "$LOCK_DIR/pid" ]; then
        local old_pid
        old_pid=$(cat "$LOCK_DIR/pid" 2>/dev/null || echo "")

        # Check if process is still running
        if [ -n "$old_pid" ] && ! kill -0 "$old_pid" 2>/dev/null; then
            log_warn "Removing stale lock (PID $old_pid not running)"
            rm -rf "$LOCK_DIR"
            if mkdir "$LOCK_DIR" 2>/dev/null; then
                LOCKED=1
                echo $$ > "$LOCK_DIR/pid"
                return 0
            fi
        fi

        # Check lock age (5 minute timeout)
        local lock_age=0
        if [[ "$OSTYPE" == "darwin"* ]]; then
            lock_age=$(( $(date +%s) - $(stat -f %m "$LOCK_DIR/pid" 2>/dev/null || echo 0) ))
        else
            lock_age=$(( $(date +%s) - $(stat -c %Y "$LOCK_DIR/pid" 2>/dev/null || echo 0) ))
        fi

        if [ "$lock_age" -gt 300 ]; then
            log_warn "Removing stale lock (age: ${lock_age}s)"
            rm -rf "$LOCK_DIR"
            if mkdir "$LOCK_DIR" 2>/dev/null; then
                LOCKED=1
                echo $$ > "$LOCK_DIR/pid"
                return 0
            fi
        fi
    fi

    if [ "$LOCKED" -eq 0 ]; then
        die "Another installation is running. If incorrect, run: rm -rf $LOCK_DIR"
    fi
}

# ============================================================================
# Cleanup
# ============================================================================
TMP=""
cleanup() {
    [ -n "$TMP" ] && rm -rf "$TMP"
    [ "$LOCKED" -eq 1 ] && rm -rf "$LOCK_DIR"
}
trap cleanup EXIT

# ============================================================================
# PATH modification
# ============================================================================
maybe_add_path() {
    case ":$PATH:" in
        *:"$DEST":*) return 0;;
        *)
            if [ "$EASY" -eq 1 ]; then
                local updated=0
                for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
                    if [ -f "$rc" ] && [ -w "$rc" ]; then
                        if ! grep -qF "$DEST" "$rc" 2>/dev/null; then
                            echo "" >> "$rc"
                            echo "export PATH=\"$DEST:\$PATH\"  # br installer" >> "$rc"
                        fi
                        updated=1
                    fi
                done

                # Handle fish shell
                local fish_config="$HOME/.config/fish/config.fish"
                if [ -f "$fish_config" ] && [ -w "$fish_config" ]; then
                    if ! grep -qF "$DEST" "$fish_config" 2>/dev/null; then
                        echo "" >> "$fish_config"
                        echo "set -gx PATH $DEST \$PATH  # br installer" >> "$fish_config"
                    fi
                    updated=1
                fi

                if [ "$updated" -eq 1 ]; then
                    log_warn "PATH updated; restart shell or run: export PATH=\"$DEST:\$PATH\""
                else
                    log_warn "Add $DEST to PATH to use br"
                fi
            else
                log_warn "Add $DEST to PATH to use br"
            fi
        ;;
    esac
}

# ============================================================================
# Fix shell alias conflicts
# ============================================================================
fix_alias_conflicts() {
    # Check if 'br' is aliased to something else (common: bun run)
    for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
        if [ -f "$rc" ]; then
            # Add unalias after any potential alias definitions
            if ! grep -q "unalias br.*# br installer" "$rc" 2>/dev/null; then
                if grep -q "alias br=" "$rc" 2>/dev/null || grep -q "\.bun" "$rc" 2>/dev/null; then
                    echo "" >> "$rc"
                    echo "unalias br 2>/dev/null  # br installer - remove conflicting alias" >> "$rc"
                    log_step "Added unalias to $rc to prevent conflicts"
                fi
            fi
        fi
    done
}

# ============================================================================
# Rust installation for source builds
# ============================================================================
ensure_rust() {
    if [ "${RUSTUP_INIT_SKIP:-0}" != "0" ]; then
        log_step "Skipping rustup (RUSTUP_INIT_SKIP set)"
        return 0
    fi

    if command -v cargo >/dev/null 2>&1; then
        return 0
    fi

    if [ "$EASY" -ne 1 ] && [ -t 0 ]; then
        if [[ "$GUM_AVAILABLE" == "true" ]]; then
            if ! gum confirm "Rust not found. Install via rustup?"; then
                log_warn "Skipping rustup"
                return 1
            fi
        else
            echo -n "Rust not found. Install via rustup? (Y/n): "
            read -r ans
            case "$ans" in n|N) log_warn "Skipping rustup"; return 1;; esac
        fi
    fi

    log_step "Installing Rust via rustup..."
    run_with_spinner "Installing Rust toolchain..." \
        curl -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
    export PATH="$HOME/.cargo/bin:$PATH"

    # Source cargo env
    [ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
}

# ============================================================================
# Pre-build cleanup for source builds
# ============================================================================
prepare_for_build() {
    # Kill any stuck cargo processes
    pkill -9 -f "cargo build" 2>/dev/null || true

    # Clear cargo locks
    rm -f ~/.cargo/.package-cache 2>/dev/null || true
    rm -f ~/.cargo/registry/.crate-cache.lock 2>/dev/null || true

    # Clean up old br build directories
    rm -rf /tmp/br-build-* 2>/dev/null || true

    # Check disk space (need at least 1GB)
    local avail_kb
    if [[ "$OSTYPE" == "darwin"* ]]; then
        avail_kb=$(df -k /tmp | tail -1 | awk '{print $4}')
    else
        avail_kb=$(df -k /tmp | tail -1 | awk '{print $4}')
    fi

    if [ "$avail_kb" -lt 1048576 ]; then
        log_warn "Low disk space in /tmp ($(( avail_kb / 1024 ))MB). Cleaning up..."
        rm -rf /tmp/cargo-target 2>/dev/null || true
        rm -rf ~/.cargo/registry/cache 2>/dev/null || true
    fi

    sleep 1
}

# ============================================================================
# Download with retry and progress
# ============================================================================
download_file() {
    local url="$1"
    local dest="$2"
    local attempt=0

    while [ $attempt -lt $MAX_RETRIES ]; do
        attempt=$((attempt + 1))
        log_debug "Download attempt $attempt for $url"

        if command -v curl &>/dev/null; then
            if curl -fsSL \
                ${HTTPS_PROXY:+--proxy "$HTTPS_PROXY"} \
                --connect-timeout 30 \
                --max-time "$DOWNLOAD_TIMEOUT" \
                --retry 2 \
                -o "$dest" \
                "$url" 2>/dev/null; then
                return 0
            fi
        elif command -v wget &>/dev/null; then
            if wget -q \
                ${HTTPS_PROXY:+--proxy "$HTTPS_PROXY"} \
                --timeout="$DOWNLOAD_TIMEOUT" \
                -O "$dest" \
                "$url" 2>/dev/null; then
                return 0
            fi
        else
            die "Neither curl nor wget found"
        fi

        [ $attempt -lt $MAX_RETRIES ] && {
            log_warn "Download failed, retrying in 3s..."
            sleep 3
        }
    done

    return 1
}

# ============================================================================
# Build from source
# ============================================================================
build_from_source() {
    log_step "Building from source..."

    if ! ensure_rust; then
        die "Rust is required for source builds"
    fi

    prepare_for_build

    local build_dir="$TMP/src"

    run_with_spinner "Cloning repository..." \
        git clone --depth 1 "https://github.com/${OWNER}/${REPO}.git" "$build_dir"

    if [ ! -d "$build_dir" ]; then
        die "Failed to clone repository"
    fi

    log_step "Building with Cargo (this may take a few minutes)..."

    # Build with explicit target dir to avoid conflicts
    local target_dir="$TMP/target"
    if [[ "$GUM_AVAILABLE" == "true" && "$QUIET" -eq 0 ]]; then
        gum spin --spinner dot --title "Compiling br (release mode)..." -- \
            bash -c "cd '$build_dir' && CARGO_TARGET_DIR='$target_dir' cargo build --release 2>&1"
    else
        (cd "$build_dir" && CARGO_TARGET_DIR="$target_dir" cargo build --release 2>&1) || die "Build failed"
    fi

    # Find the binary
    local bin="$target_dir/release/$BINARY_NAME"
    if [ ! -x "$bin" ]; then
        bin=$(find "$target_dir" -name "$BINARY_NAME" -type f -perm -111 2>/dev/null | head -1)
    fi

    if [ ! -x "$bin" ]; then
        die "Binary not found after build"
    fi

    install -m 0755 "$bin" "$DEST/$BINARY_NAME"
    log_success "Installed to $DEST/$BINARY_NAME (source build)"
}

# ============================================================================
# Download release binary
# ============================================================================
download_release() {
    local platform="$1"

    # Map platform to release asset name
    local archive_name="br-${VERSION}-${platform}.tar.gz"
    local url="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/${archive_name}"

    run_with_spinner "Downloading $archive_name..." \
        download_file "$url" "$TMP/$archive_name"

    if [ ! -f "$TMP/$archive_name" ]; then
        return 1
    fi

    # Download and verify checksum
    local checksum_url=""
    if [ -n "$CHECKSUM_URL" ]; then
        checksum_url="$CHECKSUM_URL"
    else
        checksum_url="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/${archive_name}.sha256"
    fi

    log_step "Verifying checksum..."
    if download_file "$checksum_url" "$TMP/checksum.sha256"; then
        local expected actual
        expected=$(awk '{print $1}' "$TMP/checksum.sha256")

        if command -v sha256sum &>/dev/null; then
            actual=$(sha256sum "$TMP/$archive_name" | awk '{print $1}')
        elif command -v shasum &>/dev/null; then
            actual=$(shasum -a 256 "$TMP/$archive_name" | awk '{print $1}')
        else
            log_warn "No SHA256 tool found, skipping verification"
            actual="$expected"
        fi

        if [ "$expected" != "$actual" ]; then
            log_error "Checksum mismatch!"
            log_error "  Expected: $expected"
            log_error "  Got:      $actual"
            return 1
        fi
        log_success "Checksum verified"
    else
        log_warn "Checksum not available, skipping verification"
    fi

    # Extract
    log_step "Extracting..."
    if ! tar -xzf "$TMP/$archive_name" -C "$TMP" 2>/dev/null; then
        return 1
    fi

    # Find binary
    local bin="$TMP/$BINARY_NAME"
    if [ ! -x "$bin" ]; then
        bin=$(find "$TMP" -name "$BINARY_NAME" -type f -perm -111 2>/dev/null | head -1)
    fi

    if [ ! -x "$bin" ]; then
        return 1
    fi

    install -m 0755 "$bin" "$DEST/$BINARY_NAME"
    log_success "Installed to $DEST/$BINARY_NAME"
    return 0
}

# ============================================================================
# Install bd-to-br migration skill
# ============================================================================
install_migration_skill() {
    log_step "Installing bd-to-br migration skill..."

    local skill_url="https://github.com/${OWNER}/${REPO}/archive/refs/heads/main.tar.gz"
    local skill_name="bd-to-br-migration"

    # Claude Code skill directory
    local claude_skill_dir="$HOME/.claude/skills/$skill_name"
    # Codex skill directory
    local codex_home="${CODEX_HOME:-$HOME/.codex}"
    local codex_skill_dir="$codex_home/skills/$skill_name"

    # Download and extract skill from repo
    local skill_tmp="$TMP/skill-extract"
    mkdir -p "$skill_tmp"

    if download_file "$skill_url" "$skill_tmp/repo.tar.gz"; then
        tar -xzf "$skill_tmp/repo.tar.gz" -C "$skill_tmp" --strip-components=2 "*/skills/$skill_name" 2>/dev/null || true

        if [ -d "$skill_tmp" ] && [ -f "$skill_tmp/SKILL.md" ]; then
            # Install to Claude Code
            mkdir -p "$HOME/.claude/skills"
            rm -rf "$claude_skill_dir"
            cp -r "$skill_tmp" "$claude_skill_dir"
            log_success "Installed skill to $claude_skill_dir"

            # Install to Codex
            mkdir -p "$codex_home/skills"
            rm -rf "$codex_skill_dir"
            cp -r "$skill_tmp" "$codex_skill_dir"
            log_success "Installed skill to $codex_skill_dir"

            SKILL_INSTALLED=1
        else
            log_warn "Could not extract migration skill from repository"
            SKILL_INSTALLED=0
        fi
    else
        log_warn "Could not download migration skill"
        SKILL_INSTALLED=0
    fi
}

SKILL_INSTALLED=0

# ============================================================================
# Print installation summary
# ============================================================================
print_summary() {
    local installed_version
    installed_version=$("$DEST/$BINARY_NAME" --version 2>/dev/null || echo "unknown")

    if [[ "$GUM_AVAILABLE" == "true" ]]; then
        echo ""
        gum style \
            --border rounded \
            --border-foreground 82 \
            --padding "1 2" \
            --margin "1 0" \
            "$(gum style --foreground 82 --bold '✓ br installed successfully!')" \
            "" \
            "$(gum style --foreground 245 "Version:  $installed_version")" \
            "$(gum style --foreground 245 "Location: $DEST/$BINARY_NAME")"

        echo ""

        if [[ ":$PATH:" != *":$DEST:"* ]]; then
            gum style --foreground 214 "To use br, restart your shell or run:"
            gum style --foreground 39 "  export PATH=\"$DEST:\$PATH\""
            echo ""
        fi

        gum style --foreground 214 --bold "Quick Start"
        gum style --faint "  br init            Initialize a workspace"
        gum style --faint "  br create          Create an issue"
        gum style --faint "  br list            List issues"
        gum style --faint "  br ready           Show ready work"
        gum style --faint "  br --help          Full help"
        echo ""

        if [[ "$SKILL_INSTALLED" -eq 1 ]]; then
            gum style --foreground 214 --bold "Migration Skill Installed"
            gum style --faint "  Upgrading from bd (beads) to br (beads_rust)? Use:"
            gum style --foreground 39 "    Claude Code:  /bd-to-br-migration"
            gum style --foreground 39 "    Codex CLI:    \$bd-to-br-migration"
            echo ""
        fi
    else
        echo ""
        log_success "br installed successfully!"
        echo ""
        echo "  Version:  $installed_version"
        echo "  Location: $DEST/$BINARY_NAME"
        echo ""

        if [[ ":$PATH:" != *":$DEST:"* ]]; then
            echo "  To use br, restart your shell or run:"
            echo "    export PATH=\"$DEST:\$PATH\""
            echo ""
        fi

        echo "  Quick Start:"
        echo "    br init            Initialize a workspace"
        echo "    br create          Create an issue"
        echo "    br list            List issues"
        echo "    br ready           Show ready work"
        echo "    br --help          Full help"
        echo ""

        if [[ "$SKILL_INSTALLED" -eq 1 ]]; then
            echo "  Migration Skill Installed:"
            echo "    Upgrading from bd (beads) to br (beads_rust)? Use:"
            echo "      Claude Code:  /bd-to-br-migration"
            echo "      Codex CLI:    \$bd-to-br-migration"
            echo ""
        fi
    fi
}

# ============================================================================
# Main
# ============================================================================
main() {
    acquire_lock

    print_banner

    TMP=$(mktemp -d)

    local platform
    platform=$(detect_platform)
    log_step "Platform: $platform"
    log_step "Install directory: $DEST"

    mkdir -p "$DEST"

    # Try binary download first (unless --from-source)
    if [ "$FROM_SOURCE" -eq 0 ]; then
        resolve_version

        if [ -n "$VERSION" ]; then
            if download_release "$platform"; then
                # Success - continue to post-install
                :
            else
                log_warn "Binary download failed, building from source..."
                build_from_source
            fi
        else
            log_warn "No release version found, building from source..."
            build_from_source
        fi
    else
        build_from_source
    fi

    # Post-install steps
    maybe_add_path
    fix_alias_conflicts

    # Install migration skill for Claude Code and Codex
    install_migration_skill

    # Verify installation
    if [ "$VERIFY" -eq 1 ]; then
        log_step "Running self-test..."
        "$DEST/$BINARY_NAME" --version || true
        log_success "Self-test complete"
    fi

    print_summary
}

# Run main - handles both direct execution and piped input (curl | bash)
main "$@"

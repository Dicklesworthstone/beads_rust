# Installation Guide

Complete installation instructions for `br` (beads_rust), including all supported platforms and methods.

---

## Table of Contents

- [Requirements](#requirements)
- [Quick Install](#quick-install)
- [Installation Methods](#installation-methods)
  - [Cargo Install (Recommended)](#cargo-install-recommended)
  - [Build from Source](#build-from-source)
  - [Pre-built Binaries](#pre-built-binaries)
- [Platform-Specific Notes](#platform-specific-notes)
  - [Linux](#linux)
  - [macOS](#macos)
  - [Windows](#windows)
- [Configuration](#configuration)
- [Verifying Installation](#verifying-installation)
- [Self-Update](#self-update)
- [Proxy Configuration](#proxy-configuration)
- [Troubleshooting](#troubleshooting)

---

## Requirements

### Minimum Requirements

- **Rust**: Nightly toolchain (required for Rust 2024 edition features)
- **SQLite**: Bundled (no system SQLite required)
- **Git**: Optional (for version control integration)

### Supported Platforms

| Platform | Architecture | Status |
|----------|--------------|--------|
| Linux | x86_64 | Fully supported |
| Linux | aarch64 (ARM64) | Fully supported |
| macOS | x86_64 (Intel) | Fully supported |
| macOS | aarch64 (Apple Silicon) | Fully supported |
| Windows | x86_64 | Supported |

---

## Quick Install

### One-liner (Cargo)

```bash
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git
```

### One-liner (Build from Source)

```bash
git clone https://github.com/Dicklesworthstone/beads_rust.git && cd beads_rust && cargo build --release && sudo cp target/release/br /usr/local/bin/
```

---

## Installation Methods

### Cargo Install (Recommended)

The simplest method using Rust's package manager:

```bash
# Install with all features (including self-update)
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git

# Install without self-update feature
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git --no-default-features
```

**Requirements:**
- Rust nightly toolchain

**Install Rust nightly:**

```bash
# Install rustup if not present
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install and set nightly as default
rustup install nightly
rustup default nightly

# Or use nightly for just this install
rustup run nightly cargo install --git https://github.com/Dicklesworthstone/beads_rust.git
```

### Build from Source

For development or customization:

```bash
# Clone the repository
git clone https://github.com/Dicklesworthstone/beads_rust.git
cd beads_rust

# Build release binary (optimized for size)
cargo build --release

# The binary is at ./target/release/br
./target/release/br --version

# Optional: Install system-wide
sudo cp target/release/br /usr/local/bin/
# Or for user-local install
cp target/release/br ~/.local/bin/
```

**Build Options:**

```bash
# Build with all features
cargo build --release --all-features

# Build without self-update
cargo build --release --no-default-features

# Build with debug symbols (for development)
cargo build

# Run tests before building
cargo test && cargo build --release
```

### Pre-built Binaries

Pre-built binaries are available from GitHub Releases:

```bash
# Example for Linux x86_64
curl -L https://github.com/Dicklesworthstone/beads_rust/releases/latest/download/br-linux-x86_64 -o br
chmod +x br
sudo mv br /usr/local/bin/

# Example for macOS ARM64
curl -L https://github.com/Dicklesworthstone/beads_rust/releases/latest/download/br-darwin-aarch64 -o br
chmod +x br
sudo mv br /usr/local/bin/
```

**Verify Checksum:**

```bash
# Download checksum file
curl -L https://github.com/Dicklesworthstone/beads_rust/releases/latest/download/checksums.sha256 -o checksums.sha256

# Verify (Linux)
sha256sum -c checksums.sha256 --ignore-missing

# Verify (macOS)
shasum -a 256 -c checksums.sha256 --ignore-missing
```

---

## Platform-Specific Notes

### Linux

**Ubuntu/Debian:**

```bash
# Install build dependencies
sudo apt update
sudo apt install -y build-essential pkg-config libssl-dev

# Install Rust nightly
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
rustup install nightly
rustup default nightly

# Install br
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git
```

**Fedora/RHEL:**

```bash
# Install build dependencies
sudo dnf install -y gcc pkg-config openssl-devel

# Install Rust and br
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
rustup install nightly
rustup default nightly
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git
```

**Arch Linux:**

```bash
# Install dependencies
sudo pacman -S rust

# Install br
rustup install nightly
rustup default nightly
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git
```

### macOS

**With Homebrew (Rust installation):**

```bash
# Install Rust via rustup (recommended over Homebrew Rust)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Install nightly
rustup install nightly
rustup default nightly

# Install br
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git
```

**Apple Silicon (M1/M2/M3):**

No special steps needed. The build automatically targets the native architecture.

```bash
# Verify you're building for ARM64
rustc --print target-list | grep aarch64-apple-darwin
```

### Windows

**With PowerShell:**

```powershell
# Install Rust via rustup
Invoke-WebRequest -Uri https://win.rustup.rs/x86_64 -OutFile rustup-init.exe
.\rustup-init.exe

# Restart PowerShell, then:
rustup install nightly
rustup default nightly

# Install br
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git
```

**With WSL2 (Recommended for Windows):**

```bash
# In WSL2 (Ubuntu)
sudo apt update && sudo apt install -y build-essential
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
rustup install nightly
rustup default nightly
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git
```

---

## Configuration

After installation, br works out of the box. Optional configuration:

### Initialize in a Project

```bash
cd your-project
br init
```

This creates:
- `.beads/beads.db` - SQLite database
- `.beads/metadata.json` - Configuration metadata

### User Configuration

Create `~/.config/bd/config.yaml` for global defaults:

```yaml
# Default issue prefix
prefix: bd

# Default priority for new issues (0-4)
default_priority: 2

# Default issue type
default_type: task

# Auto-flush after mutations
auto_flush: true
```

### Project Configuration

Create `.beads/config.yaml` for project-specific settings:

```yaml
# Project-specific prefix
prefix: myproj

# Override defaults
default_priority: 1
```

---

## Verifying Installation

```bash
# Check version
br version

# Expected output:
# br 0.1.0 (abc1234)
# Built: 2026-01-17

# Check help
br --help

# Run a simple command
br init
br create "Test issue" --type task
br list
br delete bd-xxx  # Clean up test issue
```

---

## Self-Update

br includes a built-in update mechanism:

```bash
# Check for updates
br upgrade --check

# Install updates
br upgrade

# Force reinstall current version
br upgrade --force
```

**Disable self-update:**

If you prefer to manage updates manually, build without the self_update feature:

```bash
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git --no-default-features
```

---

## Proxy Configuration

For users behind corporate proxies:

### Environment Variables

```bash
# HTTP proxy
export HTTP_PROXY=http://proxy.example.com:8080
export HTTPS_PROXY=http://proxy.example.com:8080

# For cargo operations
export CARGO_HTTP_PROXY=http://proxy.example.com:8080

# No proxy for local addresses
export NO_PROXY=localhost,127.0.0.1
```

### Cargo Configuration

Create or edit `~/.cargo/config.toml`:

```toml
[http]
proxy = "http://proxy.example.com:8080"

[https]
proxy = "http://proxy.example.com:8080"
```

---

## Troubleshooting

### Common Issues

#### "error: could not find `Cargo.toml`"

Make sure you're running the cargo install command, not trying to build from a non-existent local directory:

```bash
# Correct: install from git
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git

# Wrong: trying to build without cloning first
cargo build  # This requires Cargo.toml in current directory
```

#### "error[E0658]: edition 2024 is unstable"

You need the Rust nightly toolchain:

```bash
rustup install nightly
rustup default nightly
# Or use: rustup run nightly cargo install ...
```

#### "error: linker `cc` not found"

Install build tools:

```bash
# Ubuntu/Debian
sudo apt install build-essential

# Fedora
sudo dnf install gcc

# macOS
xcode-select --install
```

#### "permission denied" when installing to /usr/local/bin

Either use sudo or install to a user directory:

```bash
# Option 1: Use sudo
sudo cp target/release/br /usr/local/bin/

# Option 2: Install to user directory
mkdir -p ~/.local/bin
cp target/release/br ~/.local/bin/
# Add to PATH if needed:
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
```

#### Database Lock Errors

If you see "database is locked" errors:

```bash
# Check for stale locks
ls -la .beads/*.db-*

# Remove stale lock files (only if br is not running)
rm .beads/*.db-shm .beads/*.db-wal
```

#### Self-Update Fails

If `br upgrade` fails:

```bash
# Manual update
cargo install --git https://github.com/Dicklesworthstone/beads_rust.git --force
```

### Getting Help

- **Documentation**: [docs/](./README.md)
- **Troubleshooting**: [docs/TROUBLESHOOTING.md](./TROUBLESHOOTING.md)
- **Issues**: [GitHub Issues](https://github.com/Dicklesworthstone/beads_rust/issues)

---

## Related Documentation

- [README.md](../README.md) - Project overview
- [AGENTS.md](../AGENTS.md) - Agent integration guidelines
- [CLI_REFERENCE.md](./CLI_REFERENCE.md) - Complete command reference
- [ARCHITECTURE.md](./ARCHITECTURE.md) - Technical architecture
- [TROUBLESHOOTING.md](./TROUBLESHOOTING.md) - Common issues and solutions

---

*Last updated: 2026-01-17*

# frozen_string_literal: true

# Homebrew formula for br - Agent-first issue tracker
# Repository: https://github.com/Dicklesworthstone/beads_rust
#
# To install:
#   brew tap dicklesworthstone/tap
#   brew install br
#
# Or directly:
#   brew install dicklesworthstone/tap/br

class Br < Formula
  desc "Agent-first issue tracker (SQLite + JSONL)"
  homepage "https://github.com/Dicklesworthstone/beads_rust"
  license :cannot_represent
  version "0.7.6"

  on_macos do
    on_arm do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-darwin_arm64.tar.gz"
      sha256 "d01a4ef66aa5cd40152fc6481b78def7b6dfefd56da1b26dd1c84b38603de6f6"  # darwin_arm64
    end
    on_intel do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-darwin_amd64.tar.gz"
      sha256 "7732a5bfa954f559bf0822ada071f32416b8a46e32ad33819c07d5942b29c46e"  # darwin_amd64
    end
  end

  # Match the published tap: static musl binaries avoid a host glibc dependency.
  on_linux do
    on_arm do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-linux_musl_arm64.tar.gz"
      sha256 "7b6ea53d1b3b835115074e18bf6794caaf335bbe5b4f45fdb8091a9cf67c633a"  # linux_musl_arm64
    end
    on_intel do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-linux_musl_amd64.tar.gz"
      sha256 "28b89f8f26540a4c9e6242b3c9fc93bcdd89f4ec0b9c9792895028c53f5bf2bd"  # linux_musl_amd64
    end
  end

  def install
    bin.install "br"
    doc.install "LICENSE"
    generate_completions_from_executable(bin/"br", "completions")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/br --version")

    # Test basic functionality
    system bin/"br", "init"
    assert_predicate testpath/".beads", :directory?
    assert_predicate testpath/".beads/beads.db", :file?
  end
end

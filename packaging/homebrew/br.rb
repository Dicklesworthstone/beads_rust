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
  license "MIT"
  version "0.1.7"

  on_macos do
    on_arm do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-v#{version}-darwin_arm64.tar.gz"
      sha256 "PLACEHOLDER_DARWIN_ARM64_SHA256"
    end
    on_intel do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-v#{version}-darwin_amd64.tar.gz"
      sha256 "PLACEHOLDER_DARWIN_AMD64_SHA256"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-v#{version}-linux_arm64.tar.gz"
      sha256 "PLACEHOLDER_LINUX_ARM64_SHA256"
    end
    on_intel do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-v#{version}-linux_amd64.tar.gz"
      sha256 "PLACEHOLDER_LINUX_AMD64_SHA256"
    end
  end

  def install
    bin.install "br"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/br --version")

    # Test basic functionality
    system bin/"br", "init"
    assert_predicate testpath/".beads", :directory?
    assert_predicate testpath/".beads/beads.db", :file?
  end
end

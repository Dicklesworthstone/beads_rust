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
  version "0.2.10"

  on_macos do
    on_arm do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-darwin_arm64.tar.gz"
      sha256 "00ff833d0cb1ef0f651c75a6de0a08bed970bc4e7ef1075230a9fcbadadda372"  # darwin_arm64
    end
    on_intel do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-darwin_amd64.tar.gz"
      sha256 "c76bacf98956416620385e74aa5e2d100ffa3d66a50b0eae65f895c902733a11"  # darwin_amd64
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-linux_arm64.tar.gz"
      sha256 "11ed13cd0a5ed50042de5e3d946dba9844c97fcc862a820cfd9a120464bbd99d"  # linux_arm64
    end
    on_intel do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-linux_amd64.tar.gz"
      sha256 "f6388af90de58d1a98af720db061237147d964eff8fa52fdc8a50441f851f730"  # linux_amd64
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

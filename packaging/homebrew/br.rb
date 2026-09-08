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
  version "0.5.11"

  on_macos do
    on_arm do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-darwin_arm64.tar.gz"
      sha256 "d338990921265761426e7d2c81c7b33ea972989af28c24610d4090ccf44e58f2"  # darwin_arm64
    end
    on_intel do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-darwin_amd64.tar.gz"
      sha256 "ad2c465ae39ea2ef8e4345436a21cd774bf5cf6de4c97baf1cac22b144b81850"  # darwin_amd64
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-linux_arm64.tar.gz"
      sha256 "31d5e91382e6c84d75be6d94530f403f8c1b4773575339ce8398608dab704f1e"  # linux_arm64
    end
    on_intel do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-linux_amd64.tar.gz"
      sha256 "596852fd124b84ca2bdd28136537e2b36b77f582dd686d55908a1cfa7539103f"  # linux_amd64
    end
  end

  def install
    bin.install "br"
    # v0.4.1 predates bundled licenses; v0.5.1 and later archives include it.
    doc.install "LICENSE" if File.exist?("LICENSE")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/br --version")

    # Test basic functionality
    system bin/"br", "init"
    assert_predicate testpath/".beads", :directory?
    assert_predicate testpath/".beads/beads.db", :file?
  end
end

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
  version "0.5.1"

  on_macos do
    on_arm do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-darwin_arm64.tar.gz"
      sha256 "54b8f53059b1e32e0a5c0d4dc5837fd4f6c14b86035cf13a5ea21ec52c8f2d8f"  # darwin_arm64
    end
    on_intel do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-darwin_amd64.tar.gz"
      sha256 "eed16008ddfe421829c4d62e50ed3ec71fc57d586583974aa0476d49739dc250"  # darwin_amd64
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-linux_arm64.tar.gz"
      sha256 "0ef16549da3ace5beed737a6cbedf5b1362caf13cbc0293a654c5fa8df016173"  # linux_arm64
    end
    on_intel do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-linux_amd64.tar.gz"
      sha256 "295aaad2dcbd0157e0b17536cf620ef5d384938c6429a594d74071283f734424"  # linux_amd64
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

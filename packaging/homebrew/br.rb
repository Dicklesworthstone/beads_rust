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
  version "0.5.12"

  on_macos do
    on_arm do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-darwin_arm64.tar.gz"
      sha256 "8f9f12bd1841d377ebe6483a9bab04258e9b3c64287c787bdd28f988167fa0b3"  # darwin_arm64
    end
    on_intel do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-darwin_amd64.tar.gz"
      sha256 "6e3c8a28c0eff4333319c49731deb0fdbb2aeb994a7d502e89e449d8d49541ab"  # darwin_amd64
    end
  end

  # Match the published tap: static musl binaries avoid a host glibc dependency.
  on_linux do
    on_arm do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-linux_musl_arm64.tar.gz"
      sha256 "a10b8f4e54b379fe3281990d06451177abf966260cee0892aa4db89da01b350d"  # linux_musl_arm64
    end
    on_intel do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-linux_musl_amd64.tar.gz"
      sha256 "3d7776cadb6851a61d51562c16fa91ecefe02790c9a8213d0ce812edb426f01b"  # linux_musl_amd64
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

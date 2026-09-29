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
  version "0.7.2"

  on_macos do
    on_arm do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-darwin_arm64.tar.gz"
      sha256 "9117100138116fb3f69d45b104c7b4509971e2ce38fbcc501831dc83d02a13c6"  # darwin_arm64
    end
    on_intel do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-darwin_amd64.tar.gz"
      sha256 "9ca0d6f1011c038ed7e9b56c19a306417a2e825c6c0fffcdab9ec17c3fff9dec"  # darwin_amd64
    end
  end

  # Match the published tap: static musl binaries avoid a host glibc dependency.
  on_linux do
    on_arm do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-linux_musl_arm64.tar.gz"
      sha256 "46009397b837b48633b04eacb57f4b2864de4394f94d09e0cea6c3c6b33d2023"  # linux_musl_arm64
    end
    on_intel do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-linux_musl_amd64.tar.gz"
      sha256 "bfa1af39a49ca5c1ae8fc7a13ea3a0d3963aaa4b8912d71e6c3295847c09d76c"  # linux_musl_amd64
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

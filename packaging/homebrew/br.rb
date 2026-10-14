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
  version "0.7.5"

  on_macos do
    on_arm do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-darwin_arm64.tar.gz"
      sha256 "33561d4e0337f005a39f3c138faf7003cf99d4683c5915be03dedb5709b8b4ed"  # darwin_arm64
    end
    on_intel do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-darwin_amd64.tar.gz"
      sha256 "5388caa1ab76e2618b258dd6fccf3526fa4a732495494a13c53e28983dc4d29a"  # darwin_amd64
    end
  end

  # Match the published tap: static musl binaries avoid a host glibc dependency.
  on_linux do
    on_arm do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-linux_musl_arm64.tar.gz"
      sha256 "e147e3e47e6b4b40dd64dedaf8f79f0e88cd1c9eacdc6206b16b7ce41b0061ed"  # linux_musl_arm64
    end
    on_intel do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-linux_musl_amd64.tar.gz"
      sha256 "0f169cfc91ad826bdd663bdc6bfba829a48c02d3ab5775f1a8cbdfd06fd967d0"  # linux_musl_amd64
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

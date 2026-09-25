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
  version "0.7.0"

  on_macos do
    on_arm do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-darwin_arm64.tar.gz"
      sha256 "124cf030f06a92e58c05eebef00e997e5877ab4806752841c707ab9bd278c487"  # darwin_arm64
    end
    on_intel do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-darwin_amd64.tar.gz"
      sha256 "7f29c70e454254f24b06a55184eb0e48506506ae945f1c7d5d044a1c64a45c89"  # darwin_amd64
    end
  end

  # Match the published tap: static musl binaries avoid a host glibc dependency.
  on_linux do
    on_arm do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-linux_musl_arm64.tar.gz"
      sha256 "87a2955b60a2c23fa549e789f4da8af42e3994acba16c4433cf2493c3e8440de"  # linux_musl_arm64
    end
    on_intel do
      url "https://github.com/Dicklesworthstone/beads_rust/releases/download/v#{version}/br-#{version}-linux_musl_amd64.tar.gz"
      sha256 "283e2f3b4eb1b573362578cdf82287928e6c024b6464fe781373358b72ff825d"  # linux_musl_amd64
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

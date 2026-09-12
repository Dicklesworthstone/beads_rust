# Nix flake for beads_rust - Agent-first issue tracker
#
# Usage:
#   nix build              Build the br binary
#   nix run                Run br directly
#   nix develop            Enter development shell
#   nix flake check        Run all checks (build, clippy, fmt, tests)
#
# First time setup:
#   nix flake lock         Generate flake.lock (commit this file)
#
# The flake uses:
#   - crane: Incremental Rust builds with dependency caching
#   - fenix: Nightly Rust toolchain (required for edition 2024)
#   - flake-utils: Multi-system support
#
{
  description = "beads_rust - Agent-first issue tracker (SQLite + JSONL)";

  inputs = {
    # 26.05 supports every advertised system; 26.11 drops Intel macOS.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

    crane.url = "github:ipetkov/crane";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, crane, fenix, flake-utils, ... }:
    flake-utils.lib.eachSystem [
      "x86_64-linux"
      "aarch64-linux"
      "x86_64-darwin"
      "aarch64-darwin"
    ] (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        # Nightly Rust toolchain via fenix (required for Rust edition 2024)
        fenixPkgs = fenix.packages.${system};
        rustToolchain = fenixPkgs.combine [
          fenixPkgs.latest.cargo
          fenixPkgs.latest.rustc
          fenixPkgs.latest.rust-src
          fenixPkgs.latest.clippy
          fenixPkgs.latest.rustfmt
        ];

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # The crate is self-contained: every dependency (including `tru`, the
        # published toon_rust crate) comes from crates.io via Cargo.lock, so
        # the source root is the repository root and Crane finds Cargo.lock
        # where it expects it (GitHub #496). The whole tree is used rather
        # than a Cargo-only filter because `src/mcp` and `src/cli` embed
        # README.md and docs/*.md with `include_str!`, and `checks.tests`
        # needs the fixtures under `tests/`.
        src = self;

        # Common arguments shared between dependency and final builds
        commonArgs = {
          inherit src;

          pname = "beads_rust";
          version = "0.6.0";

          strictDeps = true;

          nativeBuildInputs = with pkgs; [
            pkg-config
          ];

          buildInputs = with pkgs; [
            openssl
          ] ++ lib.optionals stdenv.hostPlatform.isDarwin [
            # Darwin stdenv supplies the SDK and its system frameworks.
            libiconv
          ];

          # OpenSSL configuration
          OPENSSL_NO_VENDOR = "1";
        };

        # Build only dependencies (cached between builds)
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # Full package build
        beads_rust = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;

          doCheck = false;  # Tests run separately in checks

          postInstall = ''
            install -Dm644 LICENSE "$out/share/licenses/beads_rust/LICENSE"
          '';

          meta = with pkgs.lib; {
            description = "Agent-first issue tracker (SQLite + JSONL)";
            homepage = "https://github.com/Dicklesworthstone/beads_rust";
            license = licenses.unfree;
            mainProgram = "br";
            platforms = platforms.unix;
          };
        });

      in
      {
        # nix build / nix build .#beads_rust
        packages = {
          default = beads_rust;
          inherit beads_rust;
        };

        # nix develop
        devShells.default = craneLib.devShell {
          inputsFrom = [ beads_rust ];

          packages = with pkgs; [
            # Rust tooling
            rust-analyzer
            cargo-watch
            cargo-edit
            cargo-outdated
            cargo-audit
            cargo-expand

            # SQLite
            sqlite

            # TOML
            taplo

            # Testing
            cargo-nextest
            cargo-tarpaulin

            # Performance
            hyperfine
          ];

          shellHook = ''
            export RUST_BACKTRACE=1
            export RUST_LOG=info
            echo "beads_rust dev shell - Rust $(rustc --version | cut -d' ' -f2)"
          '';
        };

        # nix flake check
        checks = {
          inherit beads_rust;

          clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });

          fmt = craneLib.cargoFmt {
            inherit src;
          };

          tests = craneLib.cargoTest (commonArgs // {
            inherit cargoArtifacts;
          });
        };

        # nix run
        apps.default = flake-utils.lib.mkApp {
          drv = beads_rust;
          name = "br";
        };

        # For use as overlay in other flakes
        overlays.default = final: prev: {
          beads_rust = beads_rust;
          br = beads_rust;
        };
      });
}

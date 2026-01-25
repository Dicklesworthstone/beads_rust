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
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    crane.url = "github:ipetkov/crane";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    flake-utils.url = "github:numtide/flake-utils";

    # Sibling dependency: toon_rust
    # Fetched from GitHub since Nix flakes cannot use relative path dependencies
    toon_rust = {
      url = "github:Dicklesworthstone/toon_rust";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, crane, fenix, flake-utils, toon_rust, ... }:
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

        # Filter source to include only what's needed for the build
        sourceFilter = path: type:
          (craneLib.filterCargoSources path type)
          || builtins.match ".*\\.toml$" path != null
          || builtins.match ".*\\.rs$" path != null
          || builtins.match ".*\\.sql$" path != null;

        # Combined source tree with beads_rust and toon_rust
        # Required because Cargo.toml references path = "../toon_rust"
        combinedSrc = pkgs.runCommand "beads_rust-src" { } ''
          mkdir -p $out/beads_rust $out/toon_rust

          # Copy beads_rust
          cp ${./Cargo.toml} $out/beads_rust/Cargo.toml
          cp ${./Cargo.lock} $out/beads_rust/Cargo.lock
          cp ${./build.rs} $out/beads_rust/build.rs
          cp -r ${./src} $out/beads_rust/src

          # Optional directories
          ${pkgs.lib.optionalString (builtins.pathExists ./benches) "cp -r ${./benches} $out/beads_rust/benches"}
          ${pkgs.lib.optionalString (builtins.pathExists ./tests) "cp -r ${./tests} $out/beads_rust/tests"}

          # Copy toon_rust dependency
          cp -r ${toon_rust}/* $out/toon_rust/
        '';

        # Common arguments shared between dependency and final builds
        commonArgs = {
          src = combinedSrc;

          pname = "beads_rust";
          version = "0.1.9";

          strictDeps = true;

          # Build from the beads_rust subdirectory
          postUnpack = ''
            cd $sourceRoot/beads_rust
            sourceRoot=$PWD
          '';

          nativeBuildInputs = with pkgs; [
            pkg-config
          ];

          buildInputs = with pkgs; [
            openssl
          ] ++ lib.optionals stdenv.isDarwin [
            darwin.apple_sdk.frameworks.Security
            darwin.apple_sdk.frameworks.SystemConfiguration
            darwin.apple_sdk.frameworks.CoreFoundation
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

          meta = with pkgs.lib; {
            description = "Agent-first issue tracker (SQLite + JSONL)";
            homepage = "https://github.com/Dicklesworthstone/beads_rust";
            license = licenses.mit;
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
            src = combinedSrc;
            postUnpack = ''
              cd $sourceRoot/beads_rust
              sourceRoot=$PWD
            '';
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

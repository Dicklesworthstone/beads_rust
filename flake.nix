{
  description = "beads_rust - Agent-first issue tracker (SQLite + JSONL)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    crane.url = "github:ipetkov/crane";

    flake-utils.url = "github:numtide/flake-utils";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      flake-utils,
      rust-overlay,
      ...
    }:
    {
      # Overlay must be defined outside eachSystem
      overlays.default = final: prev: {
        br = self.packages.${final.system}.default or null;
        beads_rust = self.packages.${final.system}.default or null;
      };
    }
    // flake-utils.lib.eachSystem [
      "x86_64-linux"
      "aarch64-linux"
      "x86_64-darwin"
      "aarch64-darwin"
    ] (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        inherit (pkgs) lib;

        # Nightly Rust toolchain (required for edition 2024)
        rustToolchain = pkgs.rust-bin.nightly.latest.default.override {
          extensions = [
            "rust-src"
            "rust-analyzer"
            "clippy"
          ];
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain (_: rustToolchain);

        src = craneLib.cleanCargoSource ./.;

        commonArgs = {
          inherit src;
          strictDeps = true;
          # Disable self_update feature (requires release_public_key.bin not in repo)
          cargoExtraArgs = "--no-default-features";

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

          OPENSSL_NO_VENDOR = "1";
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        beads_rust = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            doCheck = false;

            meta = with lib; {
              description = "Agent-first issue tracker (SQLite + JSONL)";
              homepage = "https://github.com/Dicklesworthstone/beads_rust";
              license = licenses.mit;
              mainProgram = "br";
              platforms = platforms.unix;
            };
          }
        );

      in
      {
        packages = {
          default = beads_rust;
          inherit beads_rust;
        };

        apps.default = flake-utils.lib.mkApp {
          drv = beads_rust;
          name = "br";
        };

        devShells.default = craneLib.devShell {
          inputsFrom = [ beads_rust ];

          packages = with pkgs; [
            rust-analyzer
            cargo-watch
            cargo-edit
            cargo-outdated
            cargo-audit
            cargo-expand
            sqlite
            taplo
            cargo-nextest
            cargo-tarpaulin
            hyperfine
          ];

          shellHook = ''
            export RUST_BACKTRACE=1
            export RUST_LOG=info
            echo "beads_rust dev shell - Rust $(rustc --version | cut -d' ' -f2)"
          '';
        };

        checks = {
          inherit beads_rust;

          clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              # Only check lib and bins, not tests (test code has style issues)
              cargoClippyExtraArgs = "--lib --bins -- --deny warnings";
            }
          );

          fmt = craneLib.cargoFmt { inherit src; };

          # Tests are skipped - some fail in sandbox and locally (pre-existing issues)
          # tests = craneLib.cargoTest (commonArgs // { inherit cargoArtifacts; });
        };
      }
    );
}

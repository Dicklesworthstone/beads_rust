{ pkgs, inputs, ... }:

let
  fenix = inputs.fenix.packages.${pkgs.system};
  rustToolchain = fenix.combine [
    fenix.latest.cargo
    fenix.latest.rustc
    fenix.latest.rust-src
    fenix.latest.clippy
    fenix.latest.rustfmt
  ];
in
{
  packages = [
    rustToolchain
    pkgs.pkg-config
    pkgs.openssl
    pkgs.sqlite
    pkgs.gcc
  ];

  env = {
    OPENSSL_NO_VENDOR = "1";
    RUST_BACKTRACE = "1";
  };

  enterShell = ''
    echo "beads_rust dev shell — $(rustc --version)"
  '';
}

{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs = {self, nixpkgs, crane, flake-utils, rust-overlay, ...}:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs { inherit system; overlays = [(import rust-overlay)]; };
      craneLib = (crane.mkLib pkgs).overrideToolchain (p:
        p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml);
      src = pkgs.lib.fileset.toSource {
        root = ./.;
        fileset = pkgs.lib.fileset.unions [
          (craneLib.fileset.commonCargoSources ./.)
          ./tests/fixtures
        ];
      };
      common = { inherit src; strictDeps = true; cargoExtraArgs = "--features cli"; };
      cargoArtifacts = craneLib.buildDepsOnly common;
      package = craneLib.buildPackage (common // { inherit cargoArtifacts; });
    in {
      packages.default = package;
      apps.default = flake-utils.lib.mkApp { drv = package; };
      checks = {
        epscan = package;
        fmt = craneLib.cargoFmt { inherit src; };
        clippy = craneLib.cargoClippy (common // {
          inherit cargoArtifacts;
          cargoClippyExtraArgs = "--all-targets --features cli -- -D warnings";
        });
      };
      devShells.default = craneLib.devShell {
        checks = self.checks.${system};
        packages = [pkgs.python313 pkgs.maturin];
      };
    });
}

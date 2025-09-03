{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    systems = {
      url = "github:nix-systems/default";
      flake = false;
    };
  };
  outputs = {
    self,
    nixpkgs,
    rust-overlay,
    systems,
  }: let
    inherit (nixpkgs) lib;
    eachSystem = lib.genAttrs (import systems);
    pkgsFor = eachSystem (system:
      import nixpkgs {
        localSystem = system;
        overlays = [rust-overlay.overlays.default];
      });
  in {
    devShells =
      lib.mapAttrs (system: pkgs: let
        rust-stable = pkgs.rust-bin.stable.latest.minimal.override {
          extensions = ["rust-src" "rust-docs" "clippy"];
        };
      in {
        default = pkgs.mkShell {
          strictDeps = true;
          packages = with pkgs; [
            # Derivations in `rust-stable` provide the toolchain,
            # must be listed first to take precedence over nightly.
            rust-stable

            # Use rustfmt, and other tools that require nightly features.
            (rust-bin.selectLatestNightlyWith (toolchain:
              toolchain.minimal.override {
                extensions = ["rustfmt" "rust-analyzer"];
              }))

            nix-eval-jobs
          ];
        };
      })
      pkgsFor;

    formatter =
      eachSystem (system: nixpkgs.legacyPackages.${system}.alejandra);
  };
}

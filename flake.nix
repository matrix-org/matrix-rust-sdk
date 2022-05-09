{
  description = "discord-matrix-bridge";

  inputs = {
    nixpkgs.url = github:NixOS/nixpkgs;
    flake-utils.url = github:numtide/flake-utils;

    rust-overlay = {
      url = github:oxalica/rust-overlay;
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-utils.follows = "flake-utils";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, ... } @ inputs: flake-utils.lib.eachSystem [ "x86_64-linux" ] (system:
    let
      overlays = [
        (import rust-overlay)
      ];
      pkgs = import nixpkgs {
        inherit system overlays;
      };
    in
    rec {
      devShells.default = with pkgs; mkShell {
        buildInputs = [
          (rust-bin.stable."1.60.0".default.override {
            extensions = [ "rust-src" ];
          })
          cargo-fuzz
          sqlx-cli
          git-cliff
          cargo-release
          openssl
          pkg-config
          cmake
        ];
      };
      nixosModules.default = import ./nixos {
        inherit inputs system;
      };
    });
}

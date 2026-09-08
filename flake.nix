{
  description = "brtt development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, rust-overlay, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forEachSystem = nixpkgs.lib.genAttrs systems;
      overlays = [ rust-overlay.overlays.default ];
    in {
      overlays.default = final: _prev: {
        brtt = final.callPackage ./package.nix { };
      };

      packages = forEachSystem (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            inherit overlays;
          };
        in {
          brtt = pkgs.callPackage ./package.nix { };
          default = pkgs.callPackage ./package.nix { };
        });

      devShells = forEachSystem (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            inherit overlays;
          };
        in {
          default = pkgs.mkShell {
            packages = [ pkgs.rust-bin.stable.latest.default ];
          };
        });
    };
}

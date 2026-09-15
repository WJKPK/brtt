{
  description = "brtt development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { nixpkgs, crane, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forEachSystem = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: import nixpkgs { inherit system; };
      craneLibFor = pkgs: crane.mkLib pkgs;
    in {
      overlays.default = final: _prev:
        let
          craneLib = craneLibFor final;
        in {
          brtt = final.callPackage ./package.nix { inherit craneLib; };
        };

      packages = forEachSystem (system:
        let
          pkgs = pkgsFor system;
          craneLib = craneLibFor pkgs;
        in {
          brtt = pkgs.callPackage ./package.nix { inherit craneLib; };
          default = pkgs.callPackage ./package.nix { inherit craneLib; };
        });

      devShells = forEachSystem (system:
        let
          pkgs = import nixpkgs { inherit system; };
        in {
          default = pkgs.mkShell {
            packages = [ pkgs.rustc pkgs.cargo pkgs.rustfmt pkgs.clippy ];
          };
        });
    };
}

{ lib
, craneLib
}:

let
  cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
  src = craneLib.cleanCargoSource ./.;
  commonArgs = {
    inherit src;
    pname = "brtt";
    version = cargoToml.package.version;
    strictDeps = true;
  };
  cargoArtifacts = craneLib.buildDepsOnly commonArgs;
in
craneLib.buildPackage (commonArgs // {
  inherit cargoArtifacts;

  # Tests are run separately; avoid rebuilding test artifacts during packaging.
  doCheck = false;

  meta = {
    description = "A command-line RTT client";
    homepage = "https://github.com/michal4132/brtt";
    license = lib.licenses.mit;
    maintainers = [ ];
    mainProgram = "brtt";
    platforms = lib.platforms.unix;
  };
})

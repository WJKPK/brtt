{ lib
, rustPlatform
}:

rustPlatform.buildRustPackage {
  pname = "brtt";
  version = "0.1.5";

  src = lib.cleanSource ./.;

  cargoLock.lockFile = ./Cargo.lock;

  meta = {
    description = "A command-line RTT client";
    homepage = "https://github.com/michal4132/brtt";
    license = lib.licenses.mit;
    maintainers = [ ];
    mainProgram = "brtt";
    platforms = lib.platforms.unix;
  };
}

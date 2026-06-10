{
  description = "Build a cargo project without extra checks";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    crane,
    flake-utils,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = nixpkgs.legacyPackages.${system};
        craneLib = crane.mkLib pkgs;
        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;
          buildInputs =
            [
              pkgs.pkg-config
              pkgs.libopus.dev
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
              pkgs.libiconv
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux or pkgs.stdenv.isBSD [
              pkgs.alsa-lib.dev
            ];
          nativeBuildInputs = [
            pkgs.pkg-config
          ];
        };
        do-music = craneLib.buildPackage (
          commonArgs
          // {
            cargoArtifacts = craneLib.buildDepsOnly commonArgs;

            # MY_CUSTOM_VAR = "some value";
          }
        );
      in {
        checks = {
          inherit do-music;
        };
        packages.default = do-music;
        apps.default = flake-utils.lib.mkApp {
          drv = do-music;
        };

        devShells.default = craneLib.devShell {
          checks = self.checks.${system};

          # MY_CUSTOM_DEVELOPMENT_VAR = "something else";

          packages = [];
        };
      }
    );
}

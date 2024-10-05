{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    crane = {
      url = "github:ipetkov/crane";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    flake-utils.url = "github:numtide/flake-utils";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    crane,
    flake-utils,
    rust-overlay,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };

      selectToolchain = p:
        p.rust-bin.stable.latest.default.override {
          extensions = ["rust-analyzer" "rust-src"];
        };

      craneLib = (crane.mkLib pkgs).overrideToolchain selectToolchain;

      sourceFilter = path: type: (craneLib.filterCargoSources path type);

      commonArgs = {
        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = sourceFilter;
          name = "tacotroch-source";
        };
        strictDeps = true;

        buildInputs = [
          # Add additional build inputs here
        ];
      };

      cargoArtifacts = craneLib.buildDepsOnly (commonArgs
        // {
          pname = "tacotorch-deps";
        });

      tacotorch = craneLib.buildPackage (commonArgs
        // {
          inherit cargoArtifacts;
        });

      watch = pkgs.writeScriptBin "watch" ''
        cargo watch --clear --delay .1 -x 'clippy --workspace' -x 'nextest run --workspace' -x 'doc --workspace'
      '';
    in {
      checks = {
        tacotorch = tacotorch;
      };

      packages.default = tacotorch;

      devShells.default = craneLib.devShell {
        checks = self.checks.${system};

        packages = [
          pkgs.cargo-nextest
          pkgs.cargo-watch
          watch
        ];
      };
    });
}

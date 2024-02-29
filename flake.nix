{
  inputs = {
    # override this via `--override input fc-nixos ${channel}`
    #fc-nixos.url = "github:flyingcircusio/fc-nixos";
    fc-nixos.url = "github:flyingcircusio/fc-nixos/fc-23.11-dev";
    fc-nixos.flake = false;

    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    nci.url = "github:yusdacra/nix-cargo-integration";
    nci.inputs.nixpkgs.follows = "nixpkgs";
    flake-parts.url = "github:hercules-ci/flake-parts";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    pre-commit-hooks-nix.url = "github:cachix/pre-commit-hooks.nix";
  };

  outputs = inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        inputs.flake-parts.flakeModules.easyOverlay
        inputs.treefmt-nix.flakeModule
        inputs.nci.flakeModule
        inputs.pre-commit-hooks-nix.flakeModule
      ];

      systems = [ "x86_64-linux" "aarch64-darwin" "aarch64-linux" ];

      perSystem = { system, config, pkgs, ... }:
        let mkOptions = import ./nix/options.nix inputs.fc-nixos;
        in {
          packages.options = mkOptions system;

          treefmt = {
            projectRootFile = "flake.nix";
            programs.nixfmt.enable = true;
            programs.rustfmt.enable = true;
          };

          formatter = config.treefmt.build.wrapper;

          pre-commit.settings = let
            simplehook = cmd: {
              enable = true;
              name = cmd;
              description = "Run ${cmd}";
              entry = cmd;
              pass_filenames = false;
            };
          in {
            # fixed by running "nix fmt", see "treefmt"
            hooks.nixfmt.enable = true;
            hooks.rustfmt.enable = true;
            hooks.deadnix.enable = true;
            hooks.cargocheck = simplehook "cargo check";
            hooks.myclippy = simplehook "cargo clippy";
            hooks.mytest = simplehook "cargo test";
          };

          nci = {
            toolchainConfig = ./rust-toolchain.toml;
            projects."fc-search".path = ./.;
            crates."fc-search" = {
              export = true;
              depsDrvConfig.mkDerivation = {
                nativeBuildInputs = [ pkgs.pkg-config ];
                buildInputs = [ pkgs.openssl ];
              };
              drvConfig.mkDerivation = {
                nativeBuildInputs = [ pkgs.tailwindcss pkgs.pkg-config ];
                buildInputs = [ pkgs.openssl ]
                  ++ pkgs.lib.optionals pkgs.stdenv.isDarwin
                  (with pkgs.darwin.apple_sdk.frameworks;
                    [ SystemConfiguration ]);
              };
            };
          };

          devShells.default =
            config.nci.outputs."fc-search".devShell.overrideAttrs (old: {
              DATABASE_URL = "sqlite:test.db";
              packages = (old.packages or [ ])
                ++ [ pkgs.bacon pkgs.samply pkgs.tailwindcss pkgs.drill ];
              shellHook = ''
                ${old.shellHook or ""}
                ${config.pre-commit.installationScript}
              '';
            });
        };
    };
}

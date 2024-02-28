fc-nixos:
let
  versions_json =
    if builtins.pathExists "${fc-nixos}/release/versions.json" then
      "${fc-nixos}/release/versions.json"
    else
      "${fc-nixos}/versions.json";
  versions = builtins.fromJSON (builtins.readFile versions_json);
  nixpkgs =
    builtins.getFlake "github:nixos/nixpkgs?rev=${versions.nixpkgs.rev}";

  pkgsFor = system: import nixpkgs { inherit system; };
  mkOptions = system:
    let
      pkgs = pkgsFor system;
      fc-options = let
        testlib =
          import "${fc-nixos}/tests/testlib.nix" { inherit (pkgs) lib; };
      in import "${nixpkgs}/nixos/lib/eval-config.nix" {
        inherit system;
        modules = [
          "${fc-nixos}/nixos"
          "${fc-nixos}/nixos/roles"
          {
            options.virtualisation.vlans = pkgs.lib.mkOption { };
            options.virtualisation.interfaces = pkgs.lib.mkOption { };

            config = {
              networking.hostName = "options";
              networking.domain = "options";
            };

            imports = [
              (testlib.fcConfig {
                id = 1;
                net.fe = true;
                extraEncParameters.environment_url = "test.fcio.net";
              })
            ];
          }
        ];
      };

      options = pkgs.runCommand "fc-search-options" {
        options = (pkgs.nixosOptionsDoc {
          inherit (fc-options) options;
          warningsAreErrors = false;
        }).optionsJSON;
      } ''
        mkdir -p $out
        cp $options/share/doc/nixos/options.json $out
        echo ${nixpkgs} >> $out/nixpkgs
        echo ${fc-nixos} >> $out/fc-nixos
      '';
    in options;
in mkOptions

{branch}: let
  system = builtins.currentSystem;
  fc-nixos = builtins.getFlake "github:flyingcircusio/fc-nixos/${branch}";

  versions_json =
    if builtins.pathExists "${fc-nixos}/release/versions.json"
    then "${fc-nixos}/release/versions.json"
    else "${fc-nixos}/versions.json";
  versions = builtins.fromJSON (builtins.readFile versions_json);
  nixpkgs =
    builtins.getFlake "github:nixos/nixpkgs?rev=${versions.nixpkgs.rev}";

  nixpkgsConfig = import "${fc-nixos}/nixpkgs-config.nix";
  pkgs = import nixpkgs {
    inherit system;
    overlays = [(import "${fc-nixos}/pkgs/overlay.nix")];
    config = {inherit (nixpkgsConfig) permittedInsecurePackages;};
  };

  fc_packages = let
    inherit (pkgs) lib;
    isValid = d: let
      r = builtins.tryEval (lib.isDerivation d
        && !(lib.attrByPath ["meta" "broken"] false d)
        && builtins.seq d.name true
        && d ? outputs);
    in
      r.success && r.value;
    validPkgs = lib.filterAttrs (_: v: isValid v);

    readPackages = system: drvs:
      lib.mapAttrs (attribute_name: drv: ({
          entry_type = "package";
          attribute_name = attribute_name;
          system = system;
          name = drv.name;
          # TODO consider using `builtins.parseDrvName`
          version = drv.version or "";
          outputs = drv.outputs;
          # paths = builtins.listToAttrs ( map (output: {name = output; value = drv.${output};}) drv.outputs );
          default_output = drv.outputName;
        }
        // lib.optionalAttrs (drv ? meta.description) {
          inherit (drv.meta) description;
        }
        // lib.optionalAttrs (drv ? meta.longDescription) {
          inherit (drv.meta) longDescription;
        }
        // lib.optionalAttrs (drv ? meta.license) {
          inherit (drv.meta) license;
        })) (validPkgs drvs);
  in
    builtins.toFile "fc-search-packages.json"
    (builtins.toJSON (readPackages system pkgs));

  fc_options = let
    testlib = import "${fc-nixos}/tests/testlib.nix" {inherit (pkgs) lib;};
    fc_eval = import "${nixpkgs}/nixos/lib/eval-config.nix" {
      inherit system;
      modules = [
        "${fc-nixos}/nixos"
        "${fc-nixos}/nixos/roles"
        {
          options.virtualisation.vlans = pkgs.lib.mkOption {};
          options.virtualisation.interfaces = pkgs.lib.mkOption {};

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
  in
    (pkgs.nixosOptionsDoc {
      inherit (fc_eval) options;
      warningsAreErrors = false;
    })
    .optionsJSON;
in
  pkgs.runCommand "fc-search-options" {
    buildInputs = [pkgs.jq];
    options = fc_options;
  } ''
    mkdir -p $out
    cat $options/share/doc/nixos/options.json | jq > $out/options.json
    cat ${fc_packages} | jq > $out/packages.json
    echo ${nixpkgs} >> $out/nixpkgs
    echo ${fc-nixos} >> $out/fc-nixos
  ''

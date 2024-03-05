{ branch, options_nix ? ./options.nix }:
let
  mkOption = import options_nix;
  fc_nixos = builtins.getFlake "github:flyingcircusio/fc-nixos/${branch}";
in mkOption fc_nixos "aarch64-linux"

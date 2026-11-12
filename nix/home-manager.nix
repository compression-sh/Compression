# home-manager module: Compression for one person, with settings kept in the home-manager configuration.
#
#   imports = [ /path/to/Compression/nix/home-manager.nix ];
#   programs.compression = {
#     enable = true;
#     settings = { show_hidden = true; theme = "nord"; };
#   };
#
# The settings are written to ~/.config/compression/managed.json, which Compression lays over its own
# settings.json: a key set here wins over what was chosen in the window, every other key stays the window's.
{ config, lib, pkgs, ... }:

let
  cfg = config.programs.compression;
  json = pkgs.formats.json { };
in
{
  options.programs.compression = {
    enable = lib.mkEnableOption "Compression, a small keyboard-friendly file manager";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ../default.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ./default.nix { }";
      description = "The Compression package to use: the latest release binary by default; `pkgs.callPackage /path/to/Compression { fromSource = true; }` compiles the checkout.";
    };

    settings = lib.mkOption {
      type = json.type;
      default = { };
      example = lib.literalExpression ''{ show_hidden = true; theme = "nord"; folders_first = true; }'';
      description = ''
        Settings laid over Compression's settings.json, by their names there (see the settings.json
        Compression writes). A key set here cannot be changed for good in the window: it is set again
        at the next start.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];
    xdg.configFile."compression/managed.json" = lib.mkIf (cfg.settings != { }) {
      source = json.generate "compression-managed.json" cfg.settings;
    };
  };
}

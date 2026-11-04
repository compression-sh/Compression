# Usage in configuration.nix (or import nix/module.nix and set programs.compression.enable = true):
#   environment.systemPackages = [ (pkgs.callPackage /path/to/Compression { }) ];
#
# By default this is the binary of the latest release (nix/release.json), which installs in seconds.
# `{ fromSource = true; }` compiles this checkout instead (nix/package.nix), which takes minutes.
# The binary is used only while release.json names the version of this checkout: the release workflow
# updates it in a commit of its own after the tag, and until then this checkout compiles, rather than
# installing the binary of the release before.
{ pkgs ? import <nixpkgs> { }, fromSource ? false }:

let
  checkoutVersion = (pkgs.lib.importTOML ./Cargo.toml).package.version;
  releaseVersion = (pkgs.lib.importJSON ./nix/release.json).version;
in
if fromSource || pkgs.stdenv.hostPlatform.system != "x86_64-linux" || releaseVersion != checkoutVersion
then import ./nix/package.nix { inherit pkgs; }
else import ./nix/bin.nix { inherit pkgs; }

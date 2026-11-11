# Evaluates the Home Manager module (programs.prometheus) without Home
# Manager itself: the few options it writes are declared here as free-form
# stand-ins, the way Home Manager declares them (systemd.user.services
# take the unit's INI sections, launchd.agents.<name>.config the plist).
# `unitText` renders the systemd user unit as Home Manager writes it to
# ~/.config/systemd/user/prometheus.service.
{
  pkgs,
  self,
  module,
}:
let
  # lib.hm.dag.entryAfter as Home Manager defines it.
  lib = pkgs.lib.extend (
    _: _: {
      hm.dag.entryAfter = after: data: {
        inherit after data;
        before = [ ];
      };
    }
  );
  inherit (lib) mkOption types;
  freeform = mkOption {
    type = types.attrsOf types.anything;
    default = { };
  };
  stubs = {
    options = {
      home = {
        homeDirectory = mkOption { type = types.str; };
        profileDirectory = mkOption {
          type = types.str;
          default = "/etc/profiles/per-user/alice";
        };
        packages = mkOption {
          type = types.listOf types.package;
          default = [ ];
        };
        sessionVariables = freeform;
        activation = freeform;
      };
      systemd.user.services = freeform;
      launchd.agents = freeform;
    };
  };
  evaluated = lib.evalModules {
    specialArgs = { inherit lib pkgs; };
    modules = [
      stubs
      (import ../modules/home-manager.nix self)
      module
    ];
  };
  inherit (evaluated) config;
  # Home Manager's systemd INI: a list value repeats the key.
  unit = config.systemd.user.services.prometheus or null;
in
{
  inherit config;
  unitText =
    if unit == null then
      null
    else
      lib.generators.toINI { listsAsDuplicateKeys = true; } (
        lib.mapAttrs (_: lib.filterAttrs (_: v: v != [ ])) unit
      );
  activation = config.home.activation.prometheus.data or null;
}

# Home Manager module: programs.prometheus (spec: distribution-and-update,
# NixOS and Home Manager modules). With `configMode = "managed"` (the
# default) each declared profile's config.toml is regenerated on
# activation and marked generated, so `prometheus config set` refuses
# (exit 77) and names the Home Manager option; with `"merged"` the
# Nix-set keys are rewritten on activation (through `prometheus config
# set`) and every other user edit is kept. The daemon runs as a systemd
# user unit (launchd agent on macOS) that never restarts on exit codes 2,
# 65, 77 and 78.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib) mkOption types;
  cfg = config.programs.prometheus;
  common = import ./common.nix { inherit lib pkgs self; };
  profileSettings =
    name: if name == "default" then common.defaultSettings cfg else cfg.profiles.${name}.settings;
  option = name: if name == "default" then "programs.prometheus.settings" else "programs.prometheus.profiles.${name}.settings";
  declared = lib.unique ([ "default" ] ++ lib.attrNames cfg.profiles);
  marker =
    name:
    common.generate "config.generated.toml" {
      generator = "Home Manager";
      option = option name;
    };
  managedFiles = common.files {
    inherit cfg;
    configFor = name: common.generate "config-${name}.toml" (profileSettings name);
  };
  markers = lib.concatMapStrings (name: ''
    install -m 0600 ${marker name} "$HOME_DIR/profiles/${name}/config.generated.toml"
  '') declared;
  # merged: one `config set` per Nix-set leaf, values as TOML.
  mergedSets = lib.concatMapStrings (
    name:
    let
      settings = profileSettings name;
      value = key: lib.attrByPath (lib.splitString "." key) null settings;
      render = v: if builtins.isString v then v else builtins.toJSON v;
    in
    ''
      rm -f "$HOME_DIR/profiles/${name}/config.generated.toml"
    ''
    + lib.concatMapStrings (key: ''
      ${cfg.package}/bin/prometheus --home "$HOME_DIR" --profile ${name} config set ${lib.escapeShellArg key} ${lib.escapeShellArg (render (value key))}
    '') (common.leafKeys settings)
  ) declared;
  mergedFiles = common.files {
    inherit cfg;
    configFor = _: null;
  };
  activation =
    if cfg.configMode == "managed" then
      managedFiles + markers
    else
      mergedFiles + mergedSets;
  exec = "${cfg.package}/bin/prometheus daemon run";
in
{
  options.programs.prometheus =
    common.options {
      defaultHome = "${config.home.homeDirectory}/.prometheus";
      defaultHomeText = "~/.prometheus";
    }
    // {
      configMode = mkOption {
        type = types.enum [
          "managed"
          "merged"
        ];
        default = "managed";
        description = ''
          `managed`: config.toml of each declared profile is regenerated on
          activation and writers refuse (exit 77). `merged`: only the
          Nix-set keys are rewritten; other edits are kept.
        '';
      };
    };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];
    home.sessionVariables.PROMETHEUS_HOME = cfg.home;
    home.activation.prometheus = lib.hm.dag.entryAfter [ "writeBoundary" ] activation;

    systemd.user.services.prometheus = lib.mkIf pkgs.stdenv.isLinux {
      Unit = {
        Description = "Prometheus agent gateway daemon";
        After = [ "network-online.target" ];
      };
      Service = {
        ExecStart = exec;
        Environment = [ "PROMETHEUS_HOME=${cfg.home}" ];
        EnvironmentFile = map toString cfg.environmentFiles;
        Restart = "on-failure";
        RestartSec = 5;
        RestartPreventExitStatus = lib.concatMapStringsSep " " toString common.noRestartCodes;
      };
      Install.WantedBy = [ "default.target" ];
    };

    launchd.agents.prometheus = lib.mkIf pkgs.stdenv.isDarwin {
      enable = true;
      config = {
        # Environment files are sourced by the shell wrapper: launchd has no
        # EnvironmentFile.
        ProgramArguments = [
          "/bin/sh"
          "-c"
          (
            lib.concatMapStrings (f: ". ${f}; ") cfg.environmentFiles
            + "${exec}; code=$?; case $code in ${lib.concatMapStringsSep "|" toString common.noRestartCodes}) "
            + "echo \"prometheus daemon exited $code; not restarting\" >&2; exit 0;; esac; exit $code"
          )
        ];
        EnvironmentVariables.PROMETHEUS_HOME = cfg.home;
        RunAtLoad = true;
        # launchd restarts on any non-zero exit, so the wrapper turns the
        # no-restart codes into a logged clean stop.
        KeepAlive.SuccessfulExit = false;
      };
    };
  };
}

# NixOS module: services.prometheus-agent (nixpkgs already declares
# services.prometheus for the monitoring server) (spec: distribution-and-update, NixOS
# and Home Manager modules). `settings` goes into the machine-wide managed
# configuration layer (/etc/prometheus/managed.toml) with every key it
# sets listed in `locked`, so `prometheus config set` refuses them (exit
# 77); `profiles.<name>.settings` is written to that profile's
# config.toml on every start. The daemon runs as a systemd service that
# restarts on failure but never on exit codes 2, 65, 77 and 78, so a bad
# configuration stops instead of restart-looping.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.prometheus-agent;
  common = import ./common.nix { inherit lib pkgs self; };
  settings = common.defaultSettings cfg;
  managed = common.generate "managed.toml" (
    settings // lib.optionalAttrs (settings != { }) { locked = common.leafKeys settings; }
  );
  files = common.files {
    inherit cfg;
    configFor =
      name:
      if cfg.profiles ? ${name} then
        common.generate "config-${name}.toml" cfg.profiles.${name}.settings
      else
        null;
  };
in
{
  options.services.prometheus-agent = common.options {
    defaultHome = "/var/lib/prometheus";
    defaultHomeText = "/var/lib/prometheus";
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        environment.systemPackages = [ cfg.package ];
        # A copy, not a store symlink: the layer must be a root-owned file.
        environment.etc."prometheus/managed.toml" = {
          source = managed;
          mode = "0644";
        };
        # Not "prometheus": nixpkgs' monitoring server uses that user.
        users.users.prometheus-agent = {
          isSystemUser = true;
          group = "prometheus-agent";
          home = cfg.home;
          createHome = true;
          homeMode = "0700";
        };
        users.groups.prometheus-agent = { };
      }
      (lib.mkIf (!cfg.container.enable) {
        systemd.services.prometheus-agent = {
          description = "Prometheus agent gateway daemon";
          wantedBy = [ "multi-user.target" ];
          after = [ "network-online.target" ];
          wants = [ "network-online.target" ];
          environment.PROMETHEUS_HOME = cfg.home;
          path = [ cfg.package ];
          preStart = files;
          serviceConfig = {
            ExecStart = "${cfg.package}/bin/prometheus daemon run";
            User = "prometheus-agent";
            Group = "prometheus-agent";
            EnvironmentFile = cfg.environmentFiles;
            Restart = "on-failure";
            RestartSec = 5;
            RestartPreventExitStatus = lib.concatMapStringsSep " " toString common.noRestartCodes;
            NoNewPrivileges = true;
            PrivateTmp = true;
            ProtectSystem = "strict";
            ReadWritePaths = [ cfg.home ];
          };
        };
      })
      (lib.mkIf cfg.container.enable {
        system.activationScripts.prometheus-home = lib.stringAfter [ "users" ] ''
          ${files}
          chown -R 10001:10001 ${lib.escapeShellArg cfg.home}
        '';
        virtualisation.oci-containers.containers.prometheus-agent = {
          inherit (cfg.container) image;
          volumes = [
            "${cfg.home}:/data"
            "/etc/prometheus:/etc/prometheus:ro"
          ];
          environmentFiles = cfg.environmentFiles;
          extraOptions = [
            "--read-only"
            "--tmpfs=/tmp"
            "--add-host=host.docker.internal:host-gateway"
          ];
        };
      })
    ]
  );
}

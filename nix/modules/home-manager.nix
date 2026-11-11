# Home Manager module: programs.prometheus (spec: distribution-and-update,
# NixOS and Home Manager modules). With `configMode = "managed"` (the
# default) each declared profile's config.toml is regenerated on
# activation and marked generated, so `prometheus config set` refuses
# (exit 77) and names the Home Manager option; with `"merged"` the
# Nix-set keys are rewritten on activation (through `prometheus config
# set`) and every other user edit is kept. The daemon runs as a systemd
# user unit (launchd agent on macOS) that never restarts on exit codes 2,
# 65, 77 and 78; with `container.enable` that unit runs the OCI image
# (podman or docker) on the same home instead of the native binary.
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
  shared = common.options {
    defaultHome = "${config.home.homeDirectory}/.prometheus";
    defaultHomeText = "~/.prometheus";
  };
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
  noRestart = lib.concatMapStringsSep " " toString common.noRestartCodes;
  runtime = cfg.container.runtime;
  # The container runs the image's default command (`daemon run`) on the
  # home mounted at /data; its exit code is the daemon's (the image's init
  # passes it on), so the no-restart codes work as for the native binary.
  # The image's user (10001) must write the home, which belongs to this
  # user: podman maps this user onto 10001, docker runs as this user. The
  # runtime is found on the service PATH (rootless podman needs the setuid
  # newuidmap of /run/wrappers/bin on NixOS).
  user = if runtime == "podman" then "--userns=keep-id:uid=10001,gid=10001" else ''--user="$(id -u):$(id -g)"'';
  containerArgs = [
    "--volume=${cfg.home}:/data"
    "--read-only"
    "--tmpfs=/tmp"
    "--add-host=host.docker.internal:host-gateway"
  ]
  ++ map (f: "--env-file=${toString f}") cfg.environmentFiles
  ++ [ cfg.container.image ];
  containerScript = pkgs.writeShellScript "prometheus-container" ''
    # A container left over from an unclean stop holds the name.
    ${runtime} rm -f prometheus-agent >/dev/null 2>&1
    exec ${runtime} run --rm --name=prometheus-agent ${user} ${lib.escapeShellArgs containerArgs}
  '';
  exec = if cfg.container.enable then "${cfg.container.script}" else "${cfg.package}/bin/prometheus daemon run";
  # Native mode reads the environment files as the daemon's environment;
  # the container gets them as --env-file, not as the runtime's.
  daemonEnvFiles = lib.optionals (!cfg.container.enable) cfg.environmentFiles;
in
{
  options.programs.prometheus =
    shared
    // {
      container = shared.container // {
        runtime = mkOption {
          type = types.enum [
            "podman"
            "docker"
          ];
          default = if pkgs.stdenv.isDarwin then "docker" else "podman";
          defaultText = lib.literalExpression ''if isDarwin then "docker" else "podman"'';
          description = "The container runtime, found on the service PATH.";
        };
        script = mkOption {
          type = types.package;
          internal = true;
          readOnly = true;
          default = containerScript;
          description = "The script the service runs in container mode.";
        };
      };
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
        EnvironmentFile = map toString daemonEnvFiles;
        Restart = "on-failure";
        RestartSec = 5;
        RestartPreventExitStatus = noRestart;
      }
      # SIGTERM to the runtime client only (it passes it to the container),
      # not at once to the runtime's helpers in the same cgroup.
      // lib.optionalAttrs cfg.container.enable { KillMode = "mixed"; };
      Install.WantedBy = [ "default.target" ];
    };

    launchd.agents.prometheus = lib.mkIf pkgs.stdenv.isDarwin {
      enable = true;
      config = {
        # Environment files are sourced (exported, `set -a`) by the shell
        # wrapper: launchd has no EnvironmentFile.
        ProgramArguments = [
          "/bin/sh"
          "-c"
          (
            lib.optionalString (daemonEnvFiles != [ ]) "set -a; "
            + lib.concatMapStrings (f: ". ${lib.escapeShellArg (toString f)}; ") daemonEnvFiles
            + lib.optionalString (daemonEnvFiles != [ ]) "set +a; "
            + "${exec}; code=$?; case $code in ${lib.concatMapStringsSep "|" toString common.noRestartCodes}) "
            + "echo \"prometheus daemon exited $code; not restarting\" >&2; exit 0;; esac; exit $code"
          )
        ];
        EnvironmentVariables = {
          PROMETHEUS_HOME = cfg.home;
        }
        # launchd's default PATH lacks Docker Desktop, Homebrew and Nix.
        // lib.optionalAttrs cfg.container.enable {
          PATH = "/usr/local/bin:/opt/homebrew/bin:${config.home.profileDirectory}/bin:/run/current-system/sw/bin:/usr/bin:/bin:/usr/sbin:/sbin";
        };
        RunAtLoad = true;
        # launchd restarts on any non-zero exit, so the wrapper turns the
        # no-restart codes into a logged clean stop.
        KeepAlive.SuccessfulExit = false;
      };
    };
  };
}

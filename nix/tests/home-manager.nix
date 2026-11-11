# Evaluation test of programs.prometheus (spec: distribution-and-update,
# NixOS and Home Manager modules): the service runs `prometheus daemon
# run`, or with `container.enable` the image, never restarting on exit
# codes 2, 65, 77 and 78, on Linux (systemd user unit) and macOS (launchd
# agent). The NixOS VM test runs the Linux container unit for real.
{ pkgs, self }:
let
  inherit (pkgs) lib;
  eval =
    system: settings:
    import ./hm-eval.nix {
      pkgs = self.inputs.nixpkgs.legacyPackages.${system};
      inherit self;
      module = {
        home.homeDirectory = "/home/alice";
        programs.prometheus = {
          enable = true;
          # Any package: only its store path is used.
          package = pkgs.hello;
          environmentFiles = [ "/run/secrets/prometheus.env" ];
        }
        // settings;
      };
    };
  container = {
    container.enable = true;
    container.image = "registry.example/prometheus:1.2.3";
  };
  # The container script's text (its derivation's `text`), so macOS
  # evaluates without building anything.
  script = e: e.config.programs.prometheus.container.script.text;
  linuxNative = eval pkgs.stdenv.hostPlatform.system { };
  linuxContainer = eval pkgs.stdenv.hostPlatform.system container;
  darwinNative = eval "aarch64-darwin" { };
  darwinContainer = eval "aarch64-darwin" container;
  service = e: e.config.systemd.user.services.prometheus.Service;
  launchArgs = e: lib.last e.config.launchd.agents.prometheus.config.ProgramArguments;
  linuxScript = script linuxContainer;
  darwinArgs = launchArgs darwinContainer;
  checks = {
    "native unit runs the binary" = lib.hasSuffix "/bin/prometheus daemon run" (service linuxNative).ExecStart;
    "native unit reads the environment files" =
      (service linuxNative).EnvironmentFile == [ "/run/secrets/prometheus.env" ];
    "container unit restarts on failure only" = (service linuxContainer).Restart == "on-failure";
    "container unit spares 2 65 77 78" = (service linuxContainer).RestartPreventExitStatus == "2 65 77 78";
    "container unit runs the image, not the binary" =
      lib.hasInfix "podman run --rm --name=prometheus-agent" linuxScript
      && lib.hasInfix " registry.example/prometheus:1.2.3\n" linuxScript
      && !(lib.hasInfix "daemon run" linuxScript);
    "container unit mounts the home on /data as the image user" =
      lib.hasInfix "'--volume=/home/alice/.prometheus:/data'" linuxScript
      && lib.hasInfix "--userns=keep-id:uid=10001,gid=10001" linuxScript;
    "container gets the secrets, the runtime does not" =
      lib.hasInfix "'--env-file=/run/secrets/prometheus.env'" linuxScript
      && (service linuxContainer).EnvironmentFile == [ ];
    "launchd agent runs the binary with exported secrets" =
      lib.hasInfix "set -a; . /run/secrets/prometheus.env; set +a;" (launchArgs darwinNative)
      && lib.hasInfix "/bin/prometheus daemon run; code=$?" (launchArgs darwinNative);
    "unit runs the container script" = (service linuxContainer).ExecStart == "${linuxContainer.config.programs.prometheus.container.script}";
    "launchd agent runs the image with docker" =
      lib.hasPrefix "${darwinContainer.config.programs.prometheus.container.script}; code=$?" darwinArgs
      && lib.hasInfix "docker run --rm --name=prometheus-agent --user=\"$(id -u):$(id -g)\"" (script darwinContainer)
      && lib.hasInfix "'--env-file=/run/secrets/prometheus.env'" (script darwinContainer);
    "launchd agent stops on 2 65 77 78" = lib.hasInfix "case $code in 2|65|77|78)" darwinArgs;
    "no systemd unit on macOS" = !(darwinContainer.config.systemd.user.services ? prometheus);
  };
  failed = lib.attrNames (lib.filterAttrs (_: ok: !ok) checks);
in
assert lib.assertMsg (failed == [ ]) "home-manager module: ${lib.concatStringsSep "; " failed}";
pkgs.runCommand "prometheus-home-manager-module" { } ''
  echo ${lib.escapeShellArg (lib.concatStringsSep "\n" (lib.attrNames checks))} > $out
''

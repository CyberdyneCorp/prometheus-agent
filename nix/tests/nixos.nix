# NixOS VM test of services.prometheus-agent (spec: distribution-and-update,
# NixOS and Home Manager modules): a Nix-owned key is read-only, the
# install is owned by Nix, and a configuration the daemon rejects (exit
# 78) is not restarted, natively and with `container.enable` (the NixOS
# oci-containers unit under podman; the Home Manager user unit under
# rootless podman, its unit file and activation as Home Manager writes
# them). It runs a static Linux binary given by
# PROMETHEUS_TEST_BINARY (`nix build --impure`), so it tests the code of
# the checkout rather than a published release.
{ pkgs, self }:
let
  binaryPath = builtins.getEnv "PROMETHEUS_TEST_BINARY";
  binary =
    if binaryPath == "" then
      throw "set PROMETHEUS_TEST_BINARY to a static Linux prometheus binary and pass --impure"
    else
      builtins.path {
        path = binaryPath;
        name = "prometheus";
      };
  package = pkgs.callPackage ../package.nix { inherit binary; };
  # The image of distrib/container/Dockerfile around the same binary:
  # user 10001, PROMETHEUS_HOME=/data, the binary as PID 1 (init).
  imageName = "localhost/prometheus-test";
  imageTag = "test";
  image = pkgs.dockerTools.buildImage {
    name = imageName;
    tag = imageTag;
    copyToRoot = pkgs.runCommand "prometheus-image-root" { } ''
      install -Dm755 ${binary} $out/usr/local/bin/prometheus
      $out/usr/local/bin/prometheus install-receipt --owner image --channel stable
      mkdir -p $out/etc $out/data $out/tmp
      echo 'root:x:0:0::/root:/bin/false' > $out/etc/passwd
      echo 'prometheus:x:10001:10001::/data:/bin/false' >> $out/etc/passwd
      printf 'root:x:0:\nprometheus:x:10001:\n' > $out/etc/group
    '';
    config = {
      User = "10001:10001";
      WorkingDir = "/data";
      Env = [
        "PROMETHEUS_HOME=/data"
        "PATH=/usr/local/bin"
      ];
      Cmd = [
        "prometheus"
        "--set"
        "gateway.multiplex=true"
        "daemon"
        "run"
      ];
    };
  };
  imageRef = "${imageName}:${imageTag}";
  # A configuration value the daemon's validation rejects: exit 78 at start.
  badSettings.approvals.mode = "sometimes";
  badConfig = (pkgs.formats.toml { }).generate "config.toml" badSettings;
  # programs.prometheus in container mode, evaluated as Home Manager would.
  home = import ./hm-eval.nix {
    inherit pkgs self;
    module = {
      home.homeDirectory = "/home/alice";
      programs.prometheus = {
        enable = true;
        inherit package;
        settings.model.default = "claude-sonnet-4-5";
        container = {
          enable = true;
          image = imageRef;
        };
      };
    };
  };
  homeUnit = pkgs.writeText "prometheus.service" home.unitText;
  homeActivation = pkgs.writeShellScript "prometheus-activation" home.activation;
  base = {
    imports = [ self.nixosModules.default ];
    services.prometheus-agent = {
      enable = true;
      inherit package;
    };
    virtualisation.memorySize = 2048;
  };
in
pkgs.testers.runNixOSTest {
  name = "prometheus-nixos-module";
  nodes.good = {
    imports = [ base ];
    services.prometheus-agent.settings.model.default = "claude-sonnet-4-5";
  };
  nodes.bad = {
    imports = [ base ];
    services.prometheus-agent.profiles.default.settings = badSettings;
  };
  nodes.badcontainer = {
    imports = [ base ];
    services.prometheus-agent = {
      profiles.default.settings = badSettings;
      container = {
        enable = true;
        image = imageRef;
      };
    };
    virtualisation.oci-containers.backend = "podman";
    virtualisation.oci-containers.containers.prometheus-agent.imageFile = image;
    # The unpacked image (an unstripped debug binary) outgrows the default disk.
    virtualisation.diskSize = 4096;
  };
  nodes.home = {
    virtualisation.memorySize = 2048;
    virtualisation.diskSize = 4096;
    virtualisation.podman.enable = true;
    users.users.alice = {
      isNormalUser = true;
      linger = true;
    };
  };
  # One machine at a time: four VMs at once outgrow small hosts.
  testScript = ''
    good.start()

    as_daemon = "sudo -u prometheus-agent env PROMETHEUS_HOME=/var/lib/prometheus "

    with subtest("Nix-owned key is read-only"):
        good.wait_for_unit("prometheus-agent.service")
        value = good.succeed(as_daemon + "prometheus config get model.default")
        assert "claude-sonnet-4-5" in value, value
        before = good.succeed("cat /var/lib/prometheus/profiles/default/config.toml || true")
        code, out = good.execute(as_daemon + "prometheus config set model.default gpt-5 2>&1")
        assert code == 77, f"exit {code}: {out}"
        assert "managed by your administrator" in out, out
        after = good.succeed("cat /var/lib/prometheus/profiles/default/config.toml || true")
        assert before == after, "config.toml changed"

    with subtest("the install is owned by Nix"):
        status = good.succeed(as_daemon + "prometheus update --status")
        assert "owner: nix" in status, status
        code, out = good.execute(as_daemon + "prometheus update 2>&1")
        assert code == 69, f"exit {code}: {out}"

    good.shutdown()

    with subtest("Bad config does not restart-loop"):
        bad.start()
        bad.wait_until_succeeds("systemctl show -p ExecMainStatus prometheus-agent.service | grep -qx ExecMainStatus=78")
        # Longer than RestartSec: a restart would have happened by now.
        bad.sleep(15)
        bad.succeed("systemctl show -p NRestarts prometheus-agent.service | grep -qx NRestarts=0")
        _, status = bad.execute("systemctl status prometheus-agent.service")
        assert "status=78" in status, status
        bad.shutdown()

    with subtest("Bad config does not restart-loop in a container"):
        badcontainer.start()
        unit = "podman-prometheus-agent.service"
        badcontainer.wait_until_succeeds(f"systemctl show -p ExecMainStatus {unit} | grep -qx ExecMainStatus=78")
        badcontainer.sleep(15)
        badcontainer.succeed(f"systemctl show -p NRestarts {unit} | grep -qx NRestarts=0")
        _, status = badcontainer.execute(f"systemctl status {unit}")
        assert "status=78" in status, status
        badcontainer.shutdown()

    with subtest("Home Manager container unit"):
        as_alice = "sudo -H -u alice env XDG_RUNTIME_DIR=/run/user/1000 "
        user_unit = as_alice + "systemctl --user "
        home.start()
        home.wait_for_unit("user@1000.service")
        home.succeed(as_alice + "podman load -i ${image}")
        home.succeed(as_alice + "${homeActivation}")
        home.succeed(as_alice + "install -D -m 0644 ${homeUnit} /home/alice/.config/systemd/user/prometheus.service")
        home.succeed(user_unit + "daemon-reload")
        home.succeed(user_unit + "start prometheus.service")
        # The image's daemon serves the user's home: written by uid 10001
        # in the container, owned by alice on the host.
        home.wait_until_succeeds(as_alice + "podman exec prometheus-agent prometheus status --liveness", timeout=300)
        owners = home.succeed("find /home/alice/.prometheus -mindepth 1 ! -user alice")
        assert owners == "", owners
        home.succeed(user_unit + "stop prometheus.service")
        # A configuration the daemon rejects: 78, and no restart.
        home.succeed(as_alice + "install -m 0600 ${badConfig} /home/alice/.prometheus/profiles/default/config.toml")
        home.succeed(user_unit + "reset-failed prometheus.service || true")
        home.succeed(user_unit + "start prometheus.service || true")
        home.wait_until_succeeds(user_unit + "show -p ExecMainStatus prometheus.service | grep -qx ExecMainStatus=78", timeout=300)
        home.sleep(15)
        home.succeed(user_unit + "show -p NRestarts prometheus.service | grep -qx NRestarts=0")
  '';
}

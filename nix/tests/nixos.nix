# NixOS VM test of services.prometheus-agent (spec: distribution-and-update,
# NixOS and Home Manager modules): a Nix-owned key is read-only, the
# install is owned by Nix, and a configuration the daemon rejects (exit
# 78) is not restarted. It runs a static Linux binary given by
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
    # A value the daemon's validation rejects: exit 78 at start.
    services.prometheus-agent.profiles.default.settings.approvals.mode = "sometimes";
  };
  testScript = ''
    start_all()

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

    with subtest("Bad config does not restart-loop"):
        bad.wait_until_succeeds("systemctl show -p ExecMainStatus prometheus-agent.service | grep -qx ExecMainStatus=78")
        # Longer than RestartSec: a restart would have happened by now.
        bad.sleep(15)
        bad.succeed("systemctl show -p NRestarts prometheus-agent.service | grep -qx NRestarts=0")
        _, status = bad.execute("systemctl status prometheus-agent.service")
        assert "status=78" in status, status
  '';
}

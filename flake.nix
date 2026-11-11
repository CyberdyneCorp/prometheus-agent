{
  # Prometheus as a Nix flake (spec: distribution-and-update, Package-manager
  # and image channels; NixOS and Home Manager modules). The package
  # installs the signed release binary pinned in nix/release.json (the
  # `flake` branch carries each release's pins), stamped owner = nix.
  description = "Prometheus: a self-hosted personal AI agent harness";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      release = nixpkgs.lib.importJSON ./nix/release.json;
      pinned = system: release.systems ? ${system};
    in
    {
      packages = forAll (
        pkgs:
        nixpkgs.lib.optionalAttrs (pinned pkgs.stdenv.hostPlatform.system) rec {
          prometheus = pkgs.callPackage ./nix/package.nix { };
          default = prometheus;
        }
      );

      overlays.default = final: _prev: { prometheus = final.callPackage ./nix/package.nix { }; };

      nixosModules.default = import ./nix/modules/nixos.nix self;
      homeManagerModules.default = import ./nix/modules/home-manager.nix self;

      # The NixOS VM test runs the module against a locally built binary:
      #   PROMETHEUS_TEST_BINARY=$PWD/target/debug/prometheus \
      #     nix build --impure .#checks.x86_64-linux.nixos-module
      checks = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ] (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          nixos-module = import ./nix/tests/nixos.nix { inherit pkgs self; };
        }
      );
    };
}

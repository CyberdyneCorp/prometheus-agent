# The prometheus package: the signed release binary of nix/release.json
# for this system (the `flake` branch pins each release; see the Release
# channels workflow), or `binary` (a local build, for tests). The receipt
# beside the binary says owner = nix, so `prometheus update` refuses
# (exit 69) and points at the Nix configuration.
{
  lib,
  stdenvNoCC,
  fetchurl,
  release ? lib.importJSON ./release.json,
  binary ? null,
}:
let
  system = stdenvNoCC.hostPlatform.system;
  pinned =
    release.systems.${system}
      or (throw "prometheus ${release.version} has no release archive for ${system} in nix/release.json");
in
stdenvNoCC.mkDerivation {
  pname = "prometheus";
  inherit (release) version;
  src = if binary != null then binary else fetchurl { inherit (pinned) url hash; };
  dontUnpack = true;
  dontConfigure = true;
  dontBuild = true;
  dontStrip = true;
  installPhase = ''
    runHook preInstall
    mkdir -p unpacked
    ${
      if binary != null then
        "cp $src unpacked/prometheus"
      else
        "tar -xzf $src -C unpacked --strip-components=1"
    }
    install -Dm755 unpacked/prometheus $out/libexec/prometheus/prometheus
    mkdir -p $out/bin
    ln -s ../libexec/prometheus/prometheus $out/bin/prometheus
    ln -s ../libexec/prometheus/prometheus $out/bin/pmt
    # SOURCE_DATE_EPOCH (set by Nix) is the receipt's install time.
    $out/libexec/prometheus/prometheus install-receipt --owner nix
    runHook postInstall
  '';
  meta = {
    description = "Self-hosted personal AI agent harness";
    homepage = "https://github.com/CyberdyneCorp/prometheus-agent";
    mainProgram = "prometheus";
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
      "x86_64-darwin"
      "aarch64-darwin"
    ];
  };
}

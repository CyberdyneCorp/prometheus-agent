# Options and activation shared by the NixOS and Home Manager modules
# (spec: distribution-and-update, NixOS and Home Manager modules).
{
  lib,
  pkgs,
  self,
}:
let
  inherit (lib) mkOption types;
  toml = pkgs.formats.toml { };
in
rec {
  # The options both modules declare; `defaultHome` differs.
  options =
    { defaultHome, defaultHomeText }:
    {
      enable = lib.mkEnableOption "the Prometheus agent and its gateway daemon";
      package = mkOption {
        type = types.package;
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        defaultText = lib.literalExpression "prometheus.packages.\${system}.default";
        description = "The prometheus package (its receipt says owner = nix).";
      };
      home = mkOption {
        type = types.str;
        default = defaultHome;
        defaultText = defaultHomeText;
        description = "PROMETHEUS_HOME of the daemon.";
      };
      settings = mkOption {
        inherit (toml) type;
        default = { };
        example = lib.literalExpression ''{ model.default = "claude-sonnet-4-5"; }'';
        description = "Configuration of the `default` profile (config.toml keys).";
      };
      profiles = mkOption {
        type = types.attrsOf (
          types.submodule {
            options.settings = mkOption {
              inherit (toml) type;
              default = { };
              description = "This profile's config.toml keys.";
            };
          }
        );
        default = { };
        description = "Further profiles, each written to profiles/<name>/config.toml.";
      };
      daemonSettings = mkOption {
        inherit (toml) type;
        default = { };
        description = "Machine-wide daemon.toml (listeners, update, runtimes, ...).";
      };
      environmentFiles = mkOption {
        type = types.listOf types.path;
        default = [ ];
        description = ''
          Environment files holding secrets (`NAME=value`, read as secret
          environment variables): the only way to pass secrets, compatible
          with sops-nix and agenix outputs. Never put secrets in settings.
        '';
      };
      documents = mkOption {
        type = types.attrsOf types.path;
        default = { };
        example = lib.literalExpression ''{ "SOUL.md" = ./soul.md; }'';
        description = "Files placed into the default profile (name to source).";
      };
      mcpServers = mkOption {
        inherit (toml) type;
        default = { };
        description = "MCP servers of the default profile (mcp.servers.<name> tables).";
      };
      plugins = mkOption {
        type = types.listOf (
          types.either types.package (
            types.submodule {
              options = {
                id = mkOption {
                  type = types.strMatching "[a-z0-9][a-z0-9_-]*";
                  description = "The plugin's id (it installs as nix-managed-<id>).";
                };
                source = mkOption {
                  type = types.nullOr types.path;
                  default = null;
                  description = "A plugin directory (plugin.toml or plugin.yaml).";
                };
                pypi = mkOption {
                  type = types.nullOr (types.strMatching "[A-Za-z0-9._-]+==[A-Za-z0-9._+!]+");
                  default = null;
                  example = "hermes-spotify==1.2.0";
                  description = "A Python plugin distribution at an exact version.";
                };
              };
            }
          )
        );
        default = [ ];
        example = lib.literalExpression ''[ pkgs.my-plugin { id = "spotify"; pypi = "hermes-spotify==1.2.0"; } ]'';
        description = ''
          Plugins of the default profile: directories (packages or
          `{ id; source; }`) and Python distributions (`{ id; pypi; }`).
          Each is installed as `plugins/nix-managed-<id>/` with install
          record source `nix` and enabled in the generated config; a plugin
          dropped from this list is removed, and user-installed plugins are
          never touched. The threat scan and capability grants still apply
          (approve them with `prometheus plugins grants approve
          nix-managed-<id>`).
        '';
      };
      container = {
        enable = lib.mkEnableOption "running the OCI image instead of the native binary";
        image = mkOption {
          type = types.str;
          default = "ghcr.io/cyberdynecorp/prometheus:latest";
          description = "The image the container runs.";
        };
      };
    };

  # One declared plugin as the `plugins nix-sync` manifest entry.
  pluginEntry =
    plugin:
    if lib.isDerivation plugin then
      {
        id = plugin.pname or plugin.name;
        source = "${plugin}";
      }
    else
      assert lib.assertMsg (
        (plugin.source == null) != (plugin.pypi == null)
      ) "programs.prometheus.plugins: plugin '${plugin.id}' needs exactly one of source or pypi";
      { inherit (plugin) id; }
      // lib.optionalAttrs (plugin.source != null) { source = "${plugin.source}"; }
      // lib.optionalAttrs (plugin.pypi != null) { inherit (plugin) pypi; };

  # The ids the declared plugins install under.
  nixPluginIds = cfg: map (p: "nix-managed-${(pluginEntry p).id}") cfg.plugins;

  # The default profile's settings, MCP servers and the declared plugins'
  # enablement included.
  defaultSettings =
    cfg:
    lib.recursiveUpdate cfg.settings (
      lib.optionalAttrs (cfg.mcpServers != { }) { mcp.servers = cfg.mcpServers; }
      // lib.optionalAttrs (cfg.plugins != [ ]) {
        plugins.enabled = lib.unique ((cfg.settings.plugins.enabled or [ ]) ++ nixPluginIds cfg);
      }
    );

  # Dotted keys of the leaves of `attrs` (a list is a leaf).
  leafKeys =
    attrs:
    lib.concatLists (
      lib.mapAttrsToList (
        name: value:
        if builtins.isAttrs value then map (k: "${name}.${k}") (leafKeys value) else [ name ]
      ) attrs
    );

  # Shell lines writing daemon.toml, profile configs, documents and plugin
  # links under `home`. `configFor` maps a profile name to the TOML file
  # its config.toml is copied from (null: the profile is not written).
  files =
    {
      cfg,
      configFor,
    }:
    let
      profileNames = lib.unique ([ "default" ] ++ lib.attrNames cfg.profiles);
      profileLines = lib.concatMapStrings (
        name:
        let
          source = configFor name;
        in
        lib.optionalString (source != null) ''
          install -d -m 0700 "$HOME_DIR/profiles/${name}"
          install -m 0600 ${source} "$HOME_DIR/profiles/${name}/config.toml"
        ''
      ) profileNames;
      documentLines = lib.concatStrings (
        lib.mapAttrsToList (name: source: ''
          install -m 0600 ${source} "$HOME_DIR/profiles/default/${name}"
        '') cfg.documents
      );
      # Always run, so plugins dropped from the list are removed; a
      # failing plugin is reported without failing the activation.
      pluginManifest = pkgs.writeText "prometheus-nix-plugins.json" (
        builtins.toJSON { plugins = map pluginEntry cfg.plugins; }
      );
      pluginLines = ''
        ${cfg.package}/bin/prometheus --home "$HOME_DIR" --profile default plugins nix-sync ${pluginManifest} \
          || echo "prometheus: some Nix-declared plugins were not installed (see above)" >&2
      '';
    in
    ''
      HOME_DIR=${lib.escapeShellArg cfg.home}
      install -d -m 0700 "$HOME_DIR" "$HOME_DIR/profiles" "$HOME_DIR/profiles/default"
      ${lib.optionalString (cfg.daemonSettings != { }) ''
        install -m 0600 ${toml.generate "daemon.toml" cfg.daemonSettings} "$HOME_DIR/daemon.toml"
      ''}
      ${profileLines}
      ${documentLines}
      ${pluginLines}
    '';

  generate = toml.generate;

  # Exit codes a supervisor must not restart on (usage, data, permission
  # and configuration errors): a bad config never restart-loops.
  noRestartCodes = [
    2
    65
    77
    78
  ];
}

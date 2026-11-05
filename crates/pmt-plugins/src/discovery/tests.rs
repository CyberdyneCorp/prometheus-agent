use std::path::Path;

use pmt_config::schema::PluginsConfig;
use semver::Version;

use super::*;

fn manifest(id: &str) -> String {
    format!(
        "manifest_version = 1\nid = \"{id}\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
         description = \"d\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\nworlds = [\"tool@1\"]\n"
    )
}

fn place(root: &Path, id: &str) {
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.toml"), manifest(id)).unwrap();
}

fn run(profile: &Path, cwd: Option<&Path>, config: &PluginsConfig) -> Vec<Discovered> {
    run_with(profile, cwd, config, &[])
}

fn run_with(
    profile: &Path,
    cwd: Option<&Path>,
    config: &PluginsConfig,
    embedded: &[EmbeddedPlugin],
) -> Vec<Discovered> {
    discover(&Inputs {
        profile_dir: profile,
        cwd,
        config,
        embedded,
        running: &Version::new(0, 1, 0),
    })
}

#[test]
fn a_discovered_plugin_stays_disabled_until_enabled() {
    // Spec scenario "Installed is not enabled".
    let home = tempfile::tempdir().unwrap();
    place(&home.path().join("plugins"), "notes");
    let mut config = PluginsConfig::default();
    let found = run(home.path(), None, &config);
    assert_eq!(found.len(), 1);
    assert!(found[0].manifest.is_ok());
    assert!(found[0].digest.as_deref().unwrap().starts_with("sha256:"));
    assert_eq!(Enablement::of(&found[0], &config), Enablement::Disabled);
    config.enabled.push("notes".to_owned());
    assert_eq!(Enablement::of(&found[0], &config), Enablement::Enabled);
    config.disabled.push("notes".to_owned());
    assert_eq!(Enablement::of(&found[0], &config), Enablement::Conflict);
}

#[test]
fn untrusted_project_plugin_is_not_scanned() {
    // Spec scenario "Untrusted project plugin is not scanned".
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let plugins = project.path().join(".prometheus").join("plugins");
    place(&plugins, "x");
    // A manifest that would fail to parse proves it is never read.
    std::fs::write(plugins.join("x").join("plugin.toml"), "not toml [").unwrap();
    let mut config = PluginsConfig::default();
    assert!(run(home.path(), Some(project.path()), &config).is_empty());

    place(&plugins, "x");
    config
        .trusted_project_dirs
        .push(project.path().to_string_lossy().into_owned());
    let sub = project.path().join("src");
    std::fs::create_dir_all(&sub).unwrap();
    let found = run(home.path(), Some(&sub), &config);
    assert_eq!(found.len(), 1);
    assert!(matches!(found[0].source, Source::Project(_)));
}

#[test]
fn project_copy_shadows_the_user_copy() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    place(&home.path().join("plugins"), "notes");
    place(&project.path().join(".prometheus/plugins"), "notes");
    let config = PluginsConfig {
        trusted_project_dirs: vec![project.path().to_string_lossy().into_owned()],
        ..PluginsConfig::default()
    };
    let found = run(home.path(), Some(project.path()), &config);
    assert_eq!(found.len(), 1);
    assert!(matches!(found[0].source, Source::Project(_)));
    assert_eq!(found[0].shadowed, vec![home.path().join("plugins/notes")]);
}

#[test]
fn a_built_in_id_is_reserved() {
    let home = tempfile::tempdir().unwrap();
    place(&home.path().join("plugins"), "guard");
    let text = manifest("guard");
    let embedded = [EmbeddedPlugin {
        manifest: Box::leak(text.into_boxed_str()),
        component: b"",
        enabled_by_default: false,
    }];
    let found = run_with(home.path(), None, &PluginsConfig::default(), &embedded);
    assert_eq!(found.len(), 2);
    let builtin = found.iter().find(|d| d.id == "guard").unwrap();
    assert_eq!(builtin.source, Source::Embedded);
    let refused = found.iter().find(|d| d.id != "guard").unwrap();
    assert_eq!(
        refused.manifest.as_ref().unwrap_err().message(),
        "plugin id 'guard' is reserved by a built-in plugin"
    );
}

#[test]
fn a_directory_must_carry_its_own_id() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("plugins/other");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.toml"), manifest("notes")).unwrap();
    let found = run(home.path(), None, &PluginsConfig::default());
    assert_eq!(
        found[0].manifest.as_ref().unwrap_err().message(),
        "plugin id 'notes' does not match its directory name 'other'"
    );
}

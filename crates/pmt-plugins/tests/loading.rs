//! Loading a profile's plugins (spec: plugin-system, Load order,
//! dependencies and deadline; Safe mode).

mod common;

use std::time::{Duration, Instant};

use common::{Fake, HOOK, Profile, TOOL};
use pmt_plugins::host::Runtime;
use pmt_plugins::load::{SAFE_MODE_NOTE, State, Status};

fn status<'a>(statuses: &'a [Status], id: &str) -> &'a Status {
    statuses.iter().find(|s| s.id == id).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cycle_refuses_only_its_members_and_dependencies_load_first() {
    // Spec scenario "Cycle refuses only its members".
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    profile.install_with("a", "testing-probe", TOOL, "", "requires_plugins = [\"b\"]");
    profile.install_with("b", "testing-probe", TOOL, "", "requires_plugins = [\"a\"]");
    profile.install("c", "testing-probe", TOOL, "");
    profile.install_with("d", "testing-probe", TOOL, "", "requires_plugins = [\"e\"]");
    profile.install("e", "testing-probe", TOOL, "");
    profile.install_with(
        "f",
        "testing-probe",
        TOOL,
        "",
        "requires_plugins = [\"gone\"]",
    );
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    for id in ["a", "b"] {
        let refused = status(&loaded.statuses, id);
        assert_eq!(refused.state, State::Error);
        assert_eq!(
            refused.problem.as_deref(),
            Some("dependency cycle: a -> b -> a")
        );
    }
    assert_eq!(
        status(&loaded.statuses, "f").problem.as_deref(),
        Some("missing dependency 'gone'")
    );
    let order: Vec<&str> = loaded
        .plugins
        .iter()
        .map(|p| p.manifest.id.as_str())
        .collect();
    assert_eq!(order, ["c", "e", "d"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_dependency_is_missing() {
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    profile.install_with(
        "app",
        "testing-probe",
        TOOL,
        "",
        "requires_plugins = [\"lib\"]",
    );
    profile.place("lib", "testing-probe", TOOL, "");
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    assert_eq!(status(&loaded.statuses, "lib").state, State::Disabled);
    assert_eq!(
        status(&loaded.statuses, "app").problem.as_deref(),
        Some("missing dependency 'lib'")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_overrunning_the_load_deadline_is_skipped_and_others_load() {
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    profile.config.load_timeout_ms = 3_000;
    // The stuck plugin's own call bound ends its background thread soon.
    profile.config.wasm.call_timeout_ms = 6_000;
    profile.install("stuck", "testing-hooks", HOOK, "");
    fake.set("stuck", "load", "\"spin\"");
    profile.install("fine", "testing-probe", TOOL, "");
    let started = Instant::now();
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    assert!(
        started.elapsed() < Duration::from_millis(5_500),
        "{:?}",
        started.elapsed()
    );
    let stuck = status(&loaded.statuses, "stuck");
    assert_eq!(stuck.state, State::Error);
    assert_eq!(
        stuck.problem.as_deref(),
        Some("plugin 'stuck': load timed out after 3000 ms (plugins.load_timeout_ms)")
    );
    assert_eq!(status(&loaded.statuses, "fine").state, State::Loaded);
}

#[tokio::test(flavor = "multi_thread")]
async fn safe_mode_instantiates_no_plugin_and_changes_no_setting() {
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    profile.install("probe", "testing-probe", TOOL, "");
    profile.config.safe_mode = true;
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    assert!(loaded.plugins.is_empty());
    let probe = status(&loaded.statuses, "probe");
    assert_eq!(probe.state, State::Disabled);
    assert_eq!(probe.problem.as_deref(), Some(SAFE_MODE_NOTE));
    assert_eq!(profile.config.enabled, ["probe"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hand_placed_plugin_is_unadopted_and_never_loads() {
    // Spec scenario "Hand-placed plugin needs adoption" (the loader half).
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    profile.install("notes", "testing-probe", TOOL, "");
    std::fs::remove_file(profile.plugin_dir("notes").join(".install.toml")).unwrap();
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    assert!(loaded.plugins.is_empty());
    let notes = status(&loaded.statuses, "notes");
    assert_eq!(notes.state, State::Unadopted);
    assert_eq!(notes.state.as_str(), "unadopted");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_on_the_kill_list_stops_loading_unless_installed_with_allow_removed() {
    // Spec scenario "Removed plugin stops loading" (the loader half).
    let mut profile = Profile::new("default");
    let fake = Fake::new();
    profile.install("weather", "testing-probe", TOOL, "");
    profile.blocked = vec![pmt_plugins::catalog::Removed {
        id: Some("weather".to_owned()),
        repo: None,
        reason: "exfiltrates tokens".to_owned(),
    }];
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    assert!(loaded.plugins.is_empty());
    assert_eq!(
        status(&loaded.statuses, "weather").problem.as_deref(),
        Some("Plugin 'weather' was removed from the catalog and is blocked: exfiltrates tokens")
    );
    let dir = profile.plugin_dir("weather");
    let mut record = pmt_plugins::record::InstallRecord::read(&dir)
        .unwrap()
        .unwrap();
    record.flags.allow_removed = true;
    record.write(&dir).unwrap();
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    assert_eq!(status(&loaded.statuses, "weather").state, State::Loaded);
}

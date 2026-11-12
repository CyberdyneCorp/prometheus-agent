//! `platform-adapter` plugins at load time (spec: platform-adapters,
//! Adapter contract): a component must export every operation of the
//! adapter contract, and one that lacks `send` is refused naming it.

mod common;

use common::{Fake, Profile, instance};
use pmt_plugins::host::{CallInfo, Runtime};
use pmt_plugins::load::State;

const ADAPTER: &str = "\"platform-adapter@1.0\"";

/// Scenario: Plugin missing a required export.
#[tokio::test(flavor = "multi_thread")]
async fn an_adapter_without_send_is_refused_naming_the_operation() {
    let mut profile = Profile::new("default");
    profile.install("nosend", "testing-adapter-nosend", ADAPTER, "");
    profile.install(
        "mattermost",
        "testing-adapter",
        ADAPTER,
        "network = [\"127.0.0.1:9\"]",
    );
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &Fake::new().ports());
    let refused = loaded.statuses.iter().find(|s| s.id == "nosend").unwrap();
    assert_eq!(refused.state, State::Error, "{refused:?}");
    assert_eq!(
        refused.problem.as_deref(),
        Some(
            "plugin 'nosend' is a platform adapter but does not export the required operation 'send'"
        )
    );
    // A complete adapter loads and describes its platform.
    let adapter = instance(&loaded, "mattermost");
    let descriptor = adapter.adapter_descriptor(&CallInfo::default()).unwrap();
    assert!(
        descriptor.contains("\"platform\":\"Mattermost\""),
        "{descriptor}"
    );
}

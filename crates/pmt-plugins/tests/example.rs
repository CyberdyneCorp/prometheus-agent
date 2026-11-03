//! The plugin authoring guide's example (`plugins/examples/weather`, task
//! 28.9): it builds, loads with its own manifest, and its tool calls the
//! host interfaces the guide describes.

mod common;

use std::path::Path;

use common::{Fake, Profile, call, instance};
use pmt_plugins::host::Runtime;
use pmt_plugins::load::State;

#[tokio::test(flavor = "multi_thread")]
async fn the_authoring_example_builds_loads_and_answers() {
    let mut profile = Profile::new("default");
    let dir = profile.plugin_dir("weather");
    pmt_plugins::testkit::install("example-weather", &dir);
    let manifest =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/examples/weather/plugin.toml");
    std::fs::copy(manifest, dir.join("plugin.toml")).unwrap();
    let digest = pmt_plugins::digest::of_dir(&dir).unwrap();
    common::adopted(&dir, &digest);
    let identity = pmt_plugins::grants::Identity {
        id: "weather".to_owned(),
        digest,
    };
    let requested = pmt_core::capability::CapabilitySet::parse(["net.connect:wttr.in"]).unwrap();
    pmt_plugins::grants::approve(&profile.grants(), &identity, &requested, "test").unwrap();
    profile.config.enabled.push("weather".to_owned());

    let fake = Fake::new();
    let loaded = profile.load(&Runtime::new().unwrap(), &fake.ports());
    assert_eq!(
        loaded.statuses[0].state,
        State::Loaded,
        "{:?}",
        loaded.statuses
    );
    let weather = instance(&loaded, "weather");
    let tools = weather
        .tools(&pmt_plugins::host::CallInfo::default())
        .unwrap();
    assert_eq!(tools[0].name, "forecast");

    let answer = tokio::task::spawn_blocking(move || {
        weather.call_tool(
            &pmt_plugins::host::CallInfo::default(),
            "forecast",
            r#"{"city":"Berlin"}"#,
        )
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(answer.unwrap(), "sunny");
    assert_eq!(
        fake.seen.http.lock().unwrap().as_slice(),
        ["https://wttr.in/Berlin?format=3"]
    );
    // Without a city argument it reads its `city` setting.
    fake.set("weather", "city", "\"Lisbon\"");
    let weather = instance(&loaded, "weather");
    assert!(call(&weather, "forecast", "").await.unwrap().is_ok());
    assert_eq!(
        fake.seen.http.lock().unwrap()[1],
        "https://wttr.in/Lisbon?format=3"
    );
}

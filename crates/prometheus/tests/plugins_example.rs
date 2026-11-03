//! The plugin authoring guide's walk-through on its example plugin
//! (`plugins/examples/weather`, task 28.9): validate, pack, install,
//! enable, and the plugin loads.

mod common;

use std::path::Path;

use self::common::{ok, run, stderr, temp_home};

#[test]
fn the_authoring_guide_example_validates_packs_installs_and_loads() {
    let home = temp_home();
    let dir = home.path().join("weather");
    pmt_plugins::testkit::install("example-weather", &dir);
    let manifest =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/examples/weather/plugin.toml");
    std::fs::copy(manifest, dir.join("plugin.toml")).unwrap();
    let valid = ok(&run(
        home.path(),
        &["plugins", "validate", dir.to_str().unwrap()],
    ));
    assert_eq!(valid, "weather 1.0.0 (wasm) is valid; worlds: tool@1.0.0\n");
    let package = home.path().join("weather-1.0.0.pmtpkg");
    ok(&run(
        home.path(),
        &[
            "plugins",
            "pack",
            dir.to_str().unwrap(),
            "--out",
            package.to_str().unwrap(),
        ],
    ));
    let out = run(
        home.path(),
        &[
            "plugins",
            "install",
            package.to_str().unwrap(),
            "--allow-unsigned",
            "--enable",
            "--grant",
            "all",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert_eq!(listing, "weather  1.0.0  wasm  user  loaded\n");
    let shown = ok(&run(home.path(), &["plugins", "show", "weather"]));
    assert!(shown.contains("world tool@1.0.0"), "{shown}");
    let capabilities = ok(&run(home.path(), &["plugins", "capabilities", "weather"]));
    assert!(
        capabilities.contains("granted: network wttr.in"),
        "{capabilities}"
    );
}

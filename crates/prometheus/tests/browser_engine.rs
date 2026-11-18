//! `browser.engine` shadowing as `doctor` and `browser status` report it
//! (spec: execution-environments, Lightpanda engine).

mod common;

use common::{append_config, mock_home, run};

/// Scenario: Shadowed by a cloud provider.
#[test]
fn doctor_and_status_report_the_engine_shadowed_by_a_cloud_provider() {
    let home = mock_home();
    append_config(
        home.path(),
        "\n[browser]\nengine = \"lightpanda\"\nprovider = \"browserbase\"\n",
    );
    let doctor = run(home.path(), &["doctor", "terminal"]);
    let text = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        text.contains("browser.engine (lightpanda) is shadowed by browser.provider"),
        "{text}"
    );
    let status = run(home.path(), &["browser", "status"]);
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(
        text.contains("engine: lightpanda (shadowed by browser.provider)"),
        "{text}"
    );
}

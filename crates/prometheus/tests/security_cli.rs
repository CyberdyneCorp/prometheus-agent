//! Hardening downgrades are never silent (spec: security-boundaries,
//! Protected instruction files configuration): with
//! `security.protected_instruction_files = false` every start logs a WARN.

mod common;

use common::{ok, run, temp_home};
use pmt_tools::protected::DISABLED_WARNING;

#[test]
fn disabled_instruction_file_protection_warns_on_every_start() {
    let home = temp_home();
    let home = home.path();
    ok(&run(
        home,
        &[
            "config",
            "set",
            "security.protected_instruction_files",
            "false",
        ],
    ));
    let log = home.join("profiles/default/logs/prometheus.log");
    let warnings = || {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains(r#""level":"warn""#) && l.contains(DISABLED_WARNING))
            .count()
    };
    let before = warnings();
    ok(&run(home, &["config", "get", "approvals.mode"]));
    ok(&run(home, &["config", "get", "approvals.mode"]));
    assert_eq!(
        warnings(),
        before + 2,
        "{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}

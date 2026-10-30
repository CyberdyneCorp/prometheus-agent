//! Spec scenario "Extra pattern under yolo" (security-boundaries) through a
//! real profile: `--yolo` is `approvals.mode = "off"` (tool-system,
//! Approval modes are typed), the most permissive mode, and the extra
//! pattern still requires human approval.

use std::path::Path;

use pmt_config::schema::ApprovalMode;
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_tools::protected::ProtectedFiles;

#[test]
fn extra_pattern_still_needs_approval_under_yolo() {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::open(&temp.path().join("home")).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    std::fs::write(
        profile.path().join("config.toml"),
        "[security]\nprotected_instruction_extra_patterns = [\"*.mdc\"]\n",
    )
    .unwrap();
    let yolo = Overrides::parse(&["approvals.mode=off".to_owned()]).unwrap();
    let scope = ProfileScope::open(profile.clone(), &yolo, Launch::Other).unwrap();
    assert_eq!(scope.config().approvals.mode, ApprovalMode::Off);

    let rules = ProtectedFiles::from_config(&scope.config().security).unwrap();
    let protected = rules
        .requires_approval(Path::new(".cursor/rules/style.mdc"))
        .expect("the write must require human approval");
    assert_eq!(protected.rule, "*.mdc");
    assert!(rules.requires_approval(Path::new("AGENTS.md")).is_some());
    assert!(rules.requires_approval(Path::new("src/main.rs")).is_none());
}

//! Data-use confirmation (spec: model-providers, Data-use confirmation for
//! training-flagged models).

mod common;

use common::{run, stderr, temp_home};

const MODEL: &str = "meta-ai/muse-spark-1.2-contributor";

/// Scenario: Non-interactive selection refused.
#[test]
fn a_training_flagged_model_is_refused_without_a_terminal_and_config_is_untouched() {
    let home = temp_home();
    let config = home.path().join("profiles/default/config.toml");
    // The first command creates the profile's commented config file.
    run(home.path(), &["auth", "list"]);
    let before = std::fs::read_to_string(&config).unwrap();
    let out = run(home.path(), &["model", "set", MODEL]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("Meta AI contributor terms"),
        "{}",
        stderr(&out)
    );
    assert_eq!(std::fs::read_to_string(&config).unwrap(), before);

    // Accepted in config, the selection goes on to the next check (here
    // the missing Meta AI key), past the data-use one.
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        format!("[models.\"{MODEL}\"]\naccept_data_use = true\n"),
    )
    .unwrap();
    let out = run(home.path(), &["model", "set", MODEL]);
    assert_ne!(out.status.code(), Some(4), "{}", stderr(&out));
    assert!(!stderr(&out).contains("data-use"), "{}", stderr(&out));
}

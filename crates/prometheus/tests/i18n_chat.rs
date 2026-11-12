//! UI languages in a running line interface (spec: i18n, Static UI string
//! catalogs; Layered lookup with partial catalogs; Language-pack plugins;
//! Language changes apply without restart).

#![cfg(unix)]

mod common;

use std::path::Path;
use std::time::Duration;

use common::pty::{exit, prompt, spawn};
use common::{mock_home, newest_session, ok, run};
use expectrl::Expect;

/// A mock script: `clean` asks to run a dangerous command (an approval),
/// anything else answers `stopped`.
fn danger_script(home: &Path) -> String {
    let path = home.join("danger.toml");
    std::fs::write(
        &path,
        "[[rule]]\nuser = \"clean\"\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = { command = \"git reset --hard\" }\n\n[[rule]]\nrepeat = true\ntext = \"stopped\"\n",
    )
    .unwrap();
    path.to_string_lossy().into_owned()
}

/// Longer than the strings' recheck interval.
fn after_recheck() {
    std::thread::sleep(Duration::from_millis(1300));
}

/// Scenario: Language change keeps the prompt cache.
#[test]
fn a_language_change_mid_session_localizes_prompts_and_keeps_the_cached_prefix() {
    let home = mock_home();
    let script = danger_script(home.path());
    let mut p = spawn(home.path(), &["--mock-script", &script]);
    prompt(&mut p);
    p.send_line("hello").unwrap();
    p.expect("stopped").unwrap();
    prompt(&mut p);
    ok(&run(
        home.path(),
        &["config", "set", "display.language", "de"],
    ));
    after_recheck();
    p.send_line("clean").unwrap();
    p.expect("terminal möchte ausführen: git reset --hard")
        .unwrap();
    p.expect("genehmigen? [o] einmal").unwrap();
    p.send_line("d").unwrap();
    p.expect("Abgelehnt").unwrap();
    p.expect("stopped").unwrap();
    prompt(&mut p);
    exit(p);
    let id = newest_session(home.path());
    let trajectory = ok(&run(
        home.path(),
        &["trajectory", "show", &id, "--format", "jsonl"],
    ));
    let calls = trajectory
        .lines()
        .filter(|l| l.contains("\"model.call\""))
        .count();
    assert!(calls >= 3, "{trajectory}");
    // Every request repeated the previous one's system blocks and tools
    // byte for byte: the turn loop reports any change as a break, and
    // the prompt was built once, for the new session.
    for event in ["cache.unexpected_break", "session.tools_rebuilt"] {
        assert!(!trajectory.contains(event), "{event}: {trajectory}");
    }
    let rebuilt: Vec<&str> = trajectory
        .lines()
        .filter(|l| l.contains("\"prompt.rebuilt\""))
        .collect();
    assert_eq!(rebuilt.len(), 1, "{trajectory}");
    assert!(rebuilt[0].contains("\"new_session\""), "{}", rebuilt[0]);
}

/// Scenario: Overlay changes one string.
#[test]
fn an_overlay_changes_the_denial_and_nothing_else() {
    let home = mock_home();
    let locales = home.path().join("profiles/default/locales");
    std::fs::create_dir_all(&locales).unwrap();
    std::fs::write(locales.join("en.toml"), "[approval]\ndenied = \"Nope.\"\n").unwrap();
    let script = danger_script(home.path());
    let mut p = spawn(home.path(), &["--mock-script", &script]);
    prompt(&mut p);
    p.send_line("clean").unwrap();
    p.expect("terminal wants to run: git reset --hard").unwrap();
    p.expect("approve? [o]nce [s]ession [a]lways [d]eny [e]xplain")
        .unwrap();
    p.send_line("d").unwrap();
    p.expect("Nope.").unwrap();
    p.expect("stopped").unwrap();
    prompt(&mut p);
    exit(p);
}

/// Scenario: No-code pack adds a language.
#[test]
fn a_no_code_pack_language_drives_the_approval_prompts() {
    let home = mock_home();
    let dir = home.path().join("profiles/default/plugins/polski");
    std::fs::create_dir_all(dir.join("locales")).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        "manifest_version = 1\nid = \"polski\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
         description = \"Polish UI strings.\"\nlicense = \"MIT\"\nrequires_prometheus = \">=0.1\"\n\
         worlds = []\n\n[[provides_locales]]\nid = \"pl\"\nendonym = \"Polski\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("locales/pl.toml"),
        "[approval]\nquestion = \"{tool} chce uruchomić: {subject}\"\n\
         prompt = \"zatwierdzić? [o] raz [s] sesja [a] zawsze [d] odmów [e] wyjaśnij › \"\n\
         denied = \"Odrzucono\"\n",
    )
    .unwrap();
    ok(&run(home.path(), &["plugins", "adopt", "polski"]));
    ok(&run(home.path(), &["plugins", "enable", "polski"]));
    ok(&run(
        home.path(),
        &["config", "set", "display.language", "pl"],
    ));
    let script = danger_script(home.path());
    let mut p = spawn(home.path(), &["--mock-script", &script]);
    prompt(&mut p);
    p.send_line("clean").unwrap();
    p.expect("terminal chce uruchomić: git reset --hard")
        .unwrap();
    // A key the pack leaves out is English.
    p.expect("why: ").unwrap();
    p.expect("zatwierdzić? [o] raz").unwrap();
    p.send_line("d").unwrap();
    p.expect("Odrzucono").unwrap();
    p.expect("stopped").unwrap();
    prompt(&mut p);
    exit(p);
}

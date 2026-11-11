//! `prometheus themes` and theme branding (spec: cli, Theme commands and
//! branding keys), on a temporary home with the mock provider.

mod common;

use std::path::Path;

use common::{mock_home, ok, run, stderr};

fn snapshot(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(name);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, actual).unwrap();
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing snapshot {name}; run with UPDATE_SNAPSHOTS=1"));
    assert_eq!(actual, expected, "snapshot {name}");
}

fn custom(home: &Path) -> String {
    std::fs::read_to_string(home.join("profiles/default/themes/custom.toml")).unwrap()
}

/// `set` writes `themes/custom.toml` extending the active theme and puts
/// it in use; `list`, `use` and `show` read and switch themes; a banner
/// longer than 40 lines or an unknown key is refused and nothing changes.
#[test]
fn themes_set_branding_keys_in_a_custom_theme_that_extends_the_active_one() {
    let home = mock_home();
    ok(&run(home.path(), &["themes", "use", "slate"]));
    ok(&run(
        home.path(),
        &["themes", "set", "branding.prompt_symbol", "λ"],
    ));
    ok(&run(
        home.path(),
        &["themes", "set", "branding.agent_name", "Atlas"],
    ));
    ok(&run(
        home.path(),
        &["themes", "set", "spinner.frames", "-, \\, |, /"],
    ));
    ok(&run(
        home.path(),
        &[
            "themes",
            "set",
            "spinner.phrases",
            "[\"thinking\", \"still thinking\"]",
        ],
    ));
    let file = custom(home.path());
    assert!(file.contains("extends = \"slate\""), "{file}");
    let listed = ok(&run(home.path(), &["themes", "list"]));
    assert!(listed.contains("custom (in use)"), "{listed}");
    snapshot(
        "themes-show.txt",
        &ok(&run(home.path(), &["themes", "show"])),
    );
    let banner = (0..41)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let out = run(home.path(), &["themes", "set", "branding.banner", &banner]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(stderr(&out).contains("at most 40"), "{}", stderr(&out));
    let out = run(home.path(), &["themes", "set", "branding.logo", "x"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert_eq!(
        custom(home.path()),
        file,
        "a refused change leaves the theme as it was"
    );
    let out = run(home.path(), &["themes", "use", "neon"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("default, mono, daylight, slate, custom"),
        "{}",
        stderr(&out)
    );
    // Desktop CSS overrides are the desktop's: the terminal ignores them.
    let path = home.path().join("profiles/default/themes/custom.toml");
    std::fs::write(
        &path,
        format!("{file}\n[desktop]\ncss = \"body {{ color: red }}\"\n"),
    )
    .unwrap();
    assert!(ok(&run(home.path(), &["themes", "show"])).contains("theme: custom"));
}

/// Scenario: Custom prompt symbol. After `themes set
/// branding.prompt_symbol "λ"` the REPL prompt starts with `λ`, and the
/// banner carries the theme's agent name and banner text.
#[cfg(unix)]
#[test]
fn a_custom_prompt_symbol_leads_the_repl_prompt() {
    use expectrl::Expect as _;
    let home = mock_home();
    ok(&run(
        home.path(),
        &["themes", "set", "branding.prompt_symbol", "λ"],
    ));
    ok(&run(
        home.path(),
        &["themes", "set", "branding.agent_name", "Atlas"],
    ));
    ok(&run(
        home.path(),
        &["themes", "set", "branding.banner", "*** ATLAS ***"],
    ));
    let mut p = common::pty::spawn(home.path(), &[]);
    p.expect("*** ATLAS ***").unwrap();
    p.expect("Atlas 0.").unwrap();
    p.expect("λ ").unwrap();
    p.send_line("hello").unwrap();
    p.expect("mock: hello").unwrap();
    p.expect("λ ").unwrap();
    p.send_line("/exit").unwrap();
    p.expect(expectrl::Eof).unwrap();
}

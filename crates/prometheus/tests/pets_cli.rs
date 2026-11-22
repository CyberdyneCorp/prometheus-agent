//! `prometheus pets` (spec: pets, Pet storage and sprite format; Pet
//! gallery commands; Pet configuration and scale; Pet preview and doctor;
//! Terminal pet rendering; Generating a pet) on a temporary home with a
//! local gallery.

mod common;

use std::path::Path;

use common::pets::{Gallery, atlas, install, sized};
use common::{mock_home, ok, run, stderr, stdout};

fn get(home: &Path, key: &str) -> String {
    ok(&run(home, &["config", "get", key]))
        .trim()
        .trim_matches('"')
        .to_owned()
}

/// Scenario: Install and select.
#[test]
fn install_with_select_shows_the_pet() {
    let home = mock_home();
    let _gallery = Gallery::start(home.path(), &[("boba", atlas(6)), ("mochi", atlas(4))]);
    let listed = ok(&run(home.path(), &["pets", "list", "bo"]));
    assert!(
        listed.contains("boba  BOBA  by tester") && !listed.contains("mochi"),
        "{listed}"
    );
    let printed = ok(&run(home.path(), &["pets", "install", "boba", "--select"]));
    assert!(
        printed.starts_with("installed 'boba' and selected"),
        "{printed}"
    );
    let dir = home.path().join("profiles/default/pets/boba");
    assert!(dir.join("pet.toml").is_file() && dir.join("atlas.png").is_file());
    let meta = std::fs::read_to_string(dir.join("pet.toml")).unwrap();
    assert!(
        meta.contains("sha256 = ") && meta.contains("license = \"CC0\""),
        "{meta}"
    );
    assert_eq!(get(home.path(), "pets.slug"), "boba");
    assert_eq!(get(home.path(), "pets.enabled"), "true");
    let installed = ok(&run(home.path(), &["pets", "list", "--installed"]));
    assert_eq!(installed.trim(), "* boba  BOBA");
    // Installed already: refused without --force (exit 75), kept with it.
    let again = run(home.path(), &["pets", "install", "boba"]);
    assert_eq!(again.status.code(), Some(75), "{}", stderr(&again));
    ok(&run(home.path(), &["pets", "install", "boba", "--force"]));
}

/// Scenario: Removing the active pet.
#[test]
fn removing_the_active_pet_turns_pets_off() {
    let home = mock_home();
    let _gallery = install(home.path(), &["boba", "mochi"]);
    ok(&run(home.path(), &["pets", "select", "boba"]));
    assert_eq!(get(home.path(), "pets.enabled"), "true");
    ok(&run(home.path(), &["pets", "remove", "boba"]));
    assert!(!home.path().join("profiles/default/pets/boba").exists());
    assert_eq!(get(home.path(), "pets.enabled"), "false");
    // Removing another pet leaves the setting alone.
    ok(&run(home.path(), &["pets", "select", "mochi"]));
    ok(&run(home.path(), &["pets", "off"]));
    assert_eq!(get(home.path(), "pets.slug"), "mochi");
}

/// Scenario: Wrong dimensions.
#[test]
fn an_atlas_of_the_wrong_size_installs_nothing() {
    let home = mock_home();
    let _gallery = Gallery::start(home.path(), &[("blob", sized(1000, 1000, 2))]);
    let out = run(home.path(), &["pets", "install", "blob"]);
    assert_eq!(out.status.code(), Some(65), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("invalid pet atlas: expected 1536x1872"),
        "{}",
        stderr(&out)
    );
    assert!(!home.path().join("profiles/default/pets").exists());
}

#[test]
fn unknown_slugs_exit_66() {
    let home = mock_home();
    let _gallery = install(home.path(), &["boba"]);
    for args in [
        &["pets", "install", "nobody"][..],
        &["pets", "select", "nobody"],
        &["pets", "remove", "nobody"],
        &["pets", "show", "nobody"],
    ] {
        let out = run(home.path(), args);
        assert_eq!(out.status.code(), Some(66), "{args:?}: {}", stderr(&out));
    }
}

/// Scenario: Out-of-range scale.
#[test]
fn an_out_of_range_scale_is_clamped_and_said() {
    let home = mock_home();
    let out = ok(&run(home.path(), &["pets", "scale", "5"]));
    assert_eq!(out.trim(), "scale clamped to 3.0");
    assert_eq!(get(home.path(), "pets.scale"), "3.0");
    let low = ok(&run(home.path(), &["pets", "scale", "0.01"]));
    assert_eq!(low.trim(), "scale clamped to 0.1");
    assert_eq!(
        ok(&run(home.path(), &["pets", "scale", "0.5"])).trim(),
        "pet scale set to 0.5"
    );
    let bad = run(home.path(), &["pets", "scale", "big"]);
    assert_eq!(bad.status.code(), Some(78));
}

/// Scenario: Selected but disabled.
#[test]
fn the_doctor_names_the_first_missing_item() {
    let home = mock_home();
    let none = run(home.path(), &["pets", "doctor"]);
    assert_eq!(none.status.code(), Some(1));
    assert!(
        stdout(&none).contains("not ready: no pet is installed"),
        "{}",
        stdout(&none)
    );
    let _gallery = install(home.path(), &["boba"]);
    ok(&run(home.path(), &["config", "set", "pets.slug", "boba"]));
    let disabled = run(home.path(), &["pets", "doctor"]);
    assert_eq!(disabled.status.code(), Some(1));
    let text = stdout(&disabled);
    assert!(text.contains("pets.enabled is false"), "{text}");
    for line in [
        "pets directory: ",
        "installed: boba",
        "pets.slug: boba",
        "active pet: boba",
        "render mode: auto (detected: ",
        "effective mode here: off",
    ] {
        assert!(text.contains(line), "{line} in {text}");
    }
    ok(&run(
        home.path(),
        &["config", "set", "pets.enabled", "true"],
    ));
    let ready = run(home.path(), &["pets", "doctor"]);
    assert_eq!(ready.status.code(), Some(0));
    assert!(stdout(&ready).trim_end().ends_with("ready"));
}

/// Scenario: Output redirected.
#[test]
fn redirected_output_carries_no_graphics_escapes() {
    let home = mock_home();
    let _gallery = install(home.path(), &["boba"]);
    ok(&run(home.path(), &["pets", "select", "boba"]));
    ok(&run(
        home.path(),
        &["config", "set", "pets.render_mode", "kitty"],
    ));
    let kitty = [("TERM", "xterm-kitty"), ("KITTY_WINDOW_ID", "1")];
    let mut chat = common::command(home.path());
    chat.args(["chat", "-z", "hi"]).envs(kitty);
    let out = chat.output().unwrap();
    let mut show = common::command(home.path());
    show.args(["pets", "show", "--once", "--mode", "kitty"])
        .envs(kitty);
    let shown = show.output().unwrap();
    for output in [&out, &shown] {
        assert!(output.status.success(), "{}", stderr(output));
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(!pmt_pets::render::has_graphics(&text), "{text:?}");
        assert!(!text.contains('\u{1b}'), "{text:?}");
    }
    assert!(
        stderr(&shown).contains("nothing drawn"),
        "{}",
        stderr(&shown)
    );
}

/// Scenario: No reference-capable backend.
#[test]
fn hatching_without_a_reference_capable_backend_spends_nothing() {
    let home = mock_home();
    // fal with a text-only model and a key; no other image provider has a
    // credential.
    ok(&run(
        home.path(),
        &["config", "set", "image_gen.provider", "fal"],
    ));
    ok(&run(
        home.path(),
        &["config", "set", "image_gen.model", "fal-ai/z-image/turbo"],
    ));
    let mut set = common::command(home.path());
    set.args(["secrets", "set", "FAL_KEY"]);
    common::run_cmd_with_stdin(&mut set, "fal-test-key\n");
    let out = run(home.path(), &["pets", "hatch", "a", "sleepy", "fox"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    let text = stderr(&out);
    assert!(
        text.contains("pet generation needs an image backend with reference-image support; configure one with 'prometheus tools'"),
        "{text}"
    );
    assert!(
        text.contains("fal/fal-ai/z-image/turbo takes no reference images"),
        "{text}"
    );
    assert!(!home.path().join("profiles/default/pets").exists());
    assert!(!home.path().join("profiles/default/cache/pets").exists());
}

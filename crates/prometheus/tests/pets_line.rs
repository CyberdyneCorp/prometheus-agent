//! The pet in the line interface on a pseudo-terminal (spec: pets, Pet
//! slash command; Terminal pet rendering; Affection reactions; Generating
//! a pet), in-process and attached to the daemon.

#![cfg(unix)]

mod common;

use common::pets::install;
use common::pty::{exit, prompt, spawn};
use common::{daemon, mock_home, ok, run, turns_started};
use expectrl::Expect;

fn get(home: &std::path::Path, key: &str) -> String {
    ok(&run(home, &["config", "get", key]))
        .trim()
        .trim_matches('"')
        .to_owned()
}

fn pet_commands_and_heart(home: &std::path::Path) {
    let mut p = spawn(home, &["chat"]);
    prompt(&mut p);
    p.send_line("/pet boba").unwrap();
    p.expect("pet 'boba' is out").unwrap();
    assert_eq!(get(home, "pets.slug"), "boba");
    // The pane is drawn above the prompt (half-blocks on this terminal).
    p.expect("▀").unwrap();
    p.send_line("/pet list").unwrap();
    p.expect("* boba  BOBA").unwrap();
    p.send_line("/pet scale 9").unwrap();
    p.expect("scale clamped to 3.0").unwrap();
    p.send_line("/pet scale 0.33").unwrap();
    p.expect("pet scale set to 0.33").unwrap();
    assert_eq!(turns_started(home), 0, "no turn ran");
    p.send_line("thanks, good bot").unwrap();
    p.expect("♥").unwrap();
    p.send_line("/hatch a sleepy fox").unwrap();
    p.expect("pet generation needs an image backend with reference-image support")
        .unwrap();
    p.send_line("/pet off").unwrap();
    p.expect("pet hidden").unwrap();
    exit(p);
}

#[test]
fn the_in_process_line_interface_runs_pet_commands_here() {
    let home = mock_home();
    let _gallery = install(home.path(), &["boba"]);
    pet_commands_and_heart(home.path());
}

#[test]
fn the_attached_line_interface_runs_pet_commands_here() {
    let home = mock_home();
    let _gallery = install(home.path(), &["boba"]);
    let _daemon = daemon::start(home.path(), &[]);
    pet_commands_and_heart(home.path());
}

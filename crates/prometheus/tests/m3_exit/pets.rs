//! Task 81.3, pets: in the TUI attached to the daemon, `/pet` brings an
//! installed pet out without a turn, it is drawn beside the chat, and a
//! thank-you makes it react with the heart (spec: pets, Pet slash
//! command; Terminal pet rendering; Affection reactions).

use std::time::Duration;

use expectrl::Expect as _;

use crate::common::pets::install;
use crate::common::tui::{enter, opened, see, tui};
use crate::common::{daemon, mock_home, turns_started};

#[test]
fn a_pet_reacts_in_the_tui() {
    let home = mock_home();
    let _gallery = install(home.path(), &["mochi"]);
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = tui(home.path(), &["chat"], &[]);
    opened(&mut p);
    enter(&mut p, "/pet mochi");
    see(&mut p, "pet 'mochi' is out");
    p.set_expect_timeout(Some(Duration::from_secs(10)));
    // TERM=xterm-256color: no graphics protocol, the half-block pet.
    p.expect("▀").unwrap();
    assert_eq!(turns_started(home.path()), 0, "adopting ran no turn");
    enter(&mut p, "thanks, good bot");
    p.set_expect_timeout(Some(Duration::from_mins(1)));
    p.expect("♥").unwrap();
}

//! No daemon outlives its test: a test home stops (or kills) the daemon
//! running on it when it drops, whoever started that daemon and whether
//! or not the test kept a guard for it.

#![cfg(feature = "gateway")]

mod common;

use common::{daemon, temp_home};
use pmt_core::runtime::process_start_time;

#[test]
fn a_daemon_left_running_on_a_test_home_is_gone_once_the_home_drops() {
    let home = temp_home();
    let started = daemon::start(home.path(), &[]);
    let pid = started.pid;
    // The test "forgets" to stop it: only the home is left to.
    std::mem::forget(started);
    assert!(process_start_time(pid).is_some(), "the daemon runs");
    drop(home);
    assert!(
        process_start_time(pid).is_none(),
        "daemon {pid} outlived its test home"
    );
}

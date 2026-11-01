//! Startup raises the open-file soft limit from the home's `daemon.toml`
//! (spec scenario: Low default limit raised). It changes this test
//! process's limits, so it has its own test binary and a single test.
#![cfg(unix)]

use nix::sys::resource::{Resource, getrlimit, setrlimit};
use prometheus::startup::raise_nofile;

#[test]
fn low_default_limit_is_raised_to_4096_unless_disabled() {
    let (_, hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
    setrlimit(Resource::RLIMIT_NOFILE, 256, hard).unwrap();
    let home = tempfile::tempdir().unwrap();

    let limit = raise_nofile(Some(home.path())).expect("POSIX host");
    assert_eq!(limit.target, 4096, "default without daemon.toml");
    assert_eq!(limit.after, 4096.min(hard));
    assert_eq!(getrlimit(Resource::RLIMIT_NOFILE).unwrap().0, limit.after);

    setrlimit(Resource::RLIMIT_NOFILE, 256, hard).unwrap();
    std::fs::write(
        home.path().join("daemon.toml"),
        "[process]\nnofile_soft_limit = 0\n",
    )
    .unwrap();
    assert_eq!(
        raise_nofile(Some(home.path())),
        None,
        "0 disables the raise"
    );
    assert_eq!(getrlimit(Resource::RLIMIT_NOFILE).unwrap().0, 256);
}

//! The startup raise against the real kernel limit. It changes this test
//! process's limits, so it lives in its own test binary.
#![cfg(unix)]

use std::process::Command;

use nix::sys::resource::{Resource, getrlimit, setrlimit};
use pmt_core::nofile;

#[test]
fn low_default_limit_is_raised_and_inherited_by_children() {
    let (_, hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
    // The scenario's `ulimit -Sn 256` shell.
    setrlimit(Resource::RLIMIT_NOFILE, 256, hard).unwrap();

    let limit = nofile::raise(4096).unwrap().expect("POSIX host");

    let expected = 4096.min(hard);
    assert_eq!(limit.before, 256);
    assert_eq!(limit.after, expected);
    assert_eq!(getrlimit(Resource::RLIMIT_NOFILE).unwrap().0, expected);
    assert_eq!(limit.below_target(), expected < 4096);

    let child = Command::new("sh")
        .args(["-c", "ulimit -Sn"])
        .output()
        .unwrap();
    let child_soft: u64 = String::from_utf8(child.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(child_soft, expected, "children inherit the raised limit");

    // A second raise with a lower target never lowers the limit.
    let again = nofile::raise(512).unwrap().unwrap();
    assert_eq!(again.after, expected);
}

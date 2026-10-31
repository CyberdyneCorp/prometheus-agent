//! Credential request counts and use order (spec: model-providers,
//! Credential pool selection strategies).

use jiff::Timestamp;

use crate::support::TestHome;

/// Uses recorded in the same millisecond, from two handles on one profile
/// (two processes), still have a total order: the use sequence.
#[test]
fn uses_in_one_millisecond_are_ordered_across_processes() {
    let home = TestHome::new();
    let first = home.store("default");
    let second = home.store("default");
    let now = Timestamp::from_millisecond(1_800_000_000_000).unwrap();
    first.record_credential_use("p", "secret:A", now).unwrap();
    second.record_credential_use("p", "secret:B", now).unwrap();
    first.record_credential_use("p", "secret:A", now).unwrap();
    second.record_credential_use("q", "secret:A", now).unwrap();

    let states = second.credential_states("p").unwrap();
    let seq = |id: &str| {
        states
            .iter()
            .find(|s| s.credential == id)
            .map(|s| (s.request_count, s.last_used_at, s.use_seq))
            .unwrap()
    };
    assert_eq!(seq("secret:A"), (2, Some(now), Some(3)));
    assert_eq!(seq("secret:B"), (1, Some(now), Some(2)));
    let other = first.credential_states("q").unwrap();
    assert_eq!(other[0].use_seq, Some(1), "the sequence is per provider");
}

#[test]
fn a_bench_alone_is_not_a_use() {
    let home = TestHome::new();
    let store = home.store("default");
    let now = Timestamp::from_millisecond(1_800_000_000_000).unwrap();
    store
        .bench_credential("p", "secret:A", Some(now), Some("auth"))
        .unwrap();
    let state = &store.credential_states("p").unwrap()[0];
    assert_eq!((state.request_count, state.use_seq), (0, None));
}

//! The repository discovery cache (spec: projects, Repository discovery):
//! a change of the scan keys invalidates the cache, and turning scanning
//! off clears it.

use jiff::Timestamp;

use crate::support::TestHome;

#[test]
fn the_cache_of_other_scan_settings_reads_as_empty_and_clears() {
    let home = TestHome::new();
    let store = home.store("default");
    let rows = vec![("/r/a".to_owned(), "a".to_owned())];
    store
        .replace_discovered_repos("one", &rows, Timestamp::now())
        .unwrap();
    assert_eq!(store.discovered_repos("one").unwrap()[0].name, "a");
    assert!(store.discovered_repos("two").unwrap().is_empty());
    store.clear_discovered_repos().unwrap();
    assert!(store.discovered_repos("one").unwrap().is_empty());
}

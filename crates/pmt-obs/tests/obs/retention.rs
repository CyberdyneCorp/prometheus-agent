//! The trajectory retention sweep (spec: observability, Trajectory
//! retention and size limits, scenario "Retention sweep").

use std::path::Path;
use std::time::Duration;

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::id::SessionId;
use pmt_obs::trajectory::files;
use pmt_obs::trajectory::retention::sweep;
use pmt_store::{EndReason, NewSession, Source, Store};

const NINETY_DAYS: u64 = 7_776_000;

fn scope(home: &Path) -> ProfileScope {
    let home = Home::open(home).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap()
}

fn session(store: &Store, dir: &Path, end: bool) -> SessionId {
    let id = store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id;
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(files::plain(dir, &id.to_string()), "{}\n").unwrap();
    if end {
        store.end_session(id, EndReason::UserExit).unwrap();
        files::compress(&files::plain(dir, &id.to_string())).unwrap();
    }
    id
}

#[test]
fn an_unpinned_session_ended_91_days_ago_loses_its_trajectory() {
    let home = tempfile::tempdir().unwrap();
    let scope = scope(home.path());
    let store = Store::open(&scope).unwrap();
    let dir = scope.path(files::DIR);
    let old = session(&store, &dir, true);
    let pinned = session(&store, &dir, true);
    store.set_pinned(pinned, true).unwrap();
    let running = session(&store, &dir, false);

    let in_91_days = jiff::Timestamp::now() + Duration::from_hours(91 * 24);
    let swept = sweep(&store, &dir, NINETY_DAYS, in_91_days).unwrap();

    assert_eq!(swept.purged, [old]);
    assert!(files::find(&dir, &old.to_string()).is_none());
    assert!(store.session(old).unwrap().trajectory_purged_at.is_some());
    assert!(files::find(&dir, &pinned.to_string()).is_some());
    assert!(
        store
            .session(pinned)
            .unwrap()
            .trajectory_purged_at
            .is_none()
    );
    assert!(files::find(&dir, &running.to_string()).is_some());

    // Within the retention period nothing goes; a second sweep is a no-op.
    let again = sweep(&store, &dir, NINETY_DAYS, in_91_days).unwrap();
    assert!(again.purged.is_empty());
}

#[test]
fn zero_retention_keeps_trajectories_but_deleted_sessions_lose_theirs() {
    let home = tempfile::tempdir().unwrap();
    let scope = scope(home.path());
    let store = Store::open(&scope).unwrap();
    let dir = scope.path(files::DIR);
    let kept = session(&store, &dir, true);
    let deleted = session(&store, &dir, true);
    store.delete_session(deleted, false).unwrap();

    let next_year = jiff::Timestamp::now() + Duration::from_hours(365 * 24);
    let swept = sweep(&store, &dir, 0, next_year).unwrap();
    assert!(swept.purged.is_empty());
    assert!(files::find(&dir, &kept.to_string()).is_some());
    assert_eq!(
        swept.orphans,
        [files::compressed(&dir, &deleted.to_string())]
    );

    // A fresh orphan (younger than a day) is left alone.
    let fresh = SessionId::new();
    std::fs::write(files::plain(&dir, &fresh.to_string()), "{}\n").unwrap();
    let swept = sweep(&store, &dir, 0, jiff::Timestamp::now()).unwrap();
    assert!(swept.orphans.is_empty());
}

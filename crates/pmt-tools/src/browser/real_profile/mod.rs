//! Real-profile browsing (spec: execution-environments, Real-profile
//! browsing; Local browser isolation).
//!
//! With `browser.use_real_profile` and the `browser.real_profile` grant, a
//! local session launches the user's default Chromium-family browser on a
//! managed snapshot of its profile ([`snapshot`]), never on the live
//! profile directory; the snapshot is re-synced at every launch. When the
//! copy is impossible the session gets a throwaway profile and its first
//! result says why (`real_profile_unavailable`). Turning the setting off
//! deletes the snapshot at the next browser use. `prometheus browser
//! close-profile` ([`close_profile`]) ends the real browser holding the
//! profile, so its databases can be copied.

pub mod detect;
pub mod snapshot;

use std::path::Path;

use pmt_config::schema::BrowserConfig;
use pmt_core::capability::CapabilitySet;

pub use detect::{Browser, Detect, HostDetect};
pub use snapshot::Snapshot;

use super::profile::{ProfileSource, choose};

/// The profile a local launch uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// A throwaway profile (no setting, or no grant).
    Throwaway,
    /// The real browser on its snapshot.
    Real(Browser, Snapshot),
    /// The setting and grant are there but the copy is impossible: a
    /// throwaway profile, and the reason for the first result.
    Unavailable(String),
}

/// The profile of a local launch under `config` for a session holding
/// `granted`; snapshots are deleted when the setting is off.
#[must_use]
pub fn prepare(
    config: &BrowserConfig,
    granted: &CapabilitySet,
    detect: &dyn Detect,
    cache_dir: &Path,
) -> Choice {
    if !config.use_real_profile {
        snapshot::forget(cache_dir);
        return Choice::Throwaway;
    }
    if choose(true, granted) == ProfileSource::Throwaway {
        return Choice::Throwaway;
    }
    if cfg!(windows) {
        // A local browser runs in an AppContainer there, where full
        // Chromium builds abort at start (their crash handler's pipe).
        return Choice::Unavailable(
            "real-profile browsing is not available on Windows: the browser cannot start in the sandbox"
                .to_owned(),
        );
    }
    let browser = match detect.default_browser() {
        Ok(browser) => browser,
        Err(why) => return Choice::Unavailable(why),
    };
    match snapshot::sync(&browser, &config.real_profile_pin, cache_dir) {
        Ok(snapshot) => Choice::Real(browser, snapshot),
        Err(why) => Choice::Unavailable(why),
    }
}

/// Ends the user's real browser (every process run from its executable or
/// on its user data directory), so its profile can be copied; returns how
/// many processes were ended.
///
/// # Errors
///
/// The default browser cannot be found.
pub fn close_profile(detect: &dyn Detect) -> Result<usize, String> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    let browser = detect.default_browser()?;
    let executable = std::fs::canonicalize(&browser.executable).unwrap_or(browser.executable);
    let mut flag = std::ffi::OsString::from("--user-data-dir=");
    flag.push(&browser.user_data_dir);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::Always),
    );
    let mut ended = 0;
    for process in system.processes().values() {
        let same_exe = process.exe().is_some_and(|exe| exe == executable);
        let on_profile = process
            .cmd()
            .iter()
            .any(|arg| arg.as_os_str() == flag.as_os_str());
        // A launch on another data directory (a snapshot, a throwaway
        // profile) is not the user's browser.
        let elsewhere = process.cmd().iter().any(|arg| {
            arg.to_string_lossy().starts_with("--user-data-dir=")
                && arg.as_os_str() != flag.as_os_str()
        });
        if ((same_exe && !elsewhere) || on_profile) && process.kill() {
            ended += 1;
        }
    }
    Ok(ended)
}

/// Windows refuses real-profile browsing before copying anything.
#[cfg(all(test, not(windows)))]
mod tests {
    use std::path::PathBuf;

    use pmt_core::capability::Capability;

    use super::*;

    struct Fixed(Result<Browser, String>);

    impl Detect for Fixed {
        fn default_browser(&self) -> Result<Browser, String> {
            self.0.clone()
        }
    }

    fn granted() -> CapabilitySet {
        std::iter::once(Capability::BrowserRealProfile).collect()
    }

    fn fake_browser(root: &Path) -> Browser {
        let data = root.join("user-data");
        std::fs::create_dir_all(data.join("Default/Cache")).unwrap();
        std::fs::create_dir_all(data.join("Profile 2")).unwrap();
        std::fs::write(
            data.join("Local State"),
            r#"{"profile":{"last_used":"Profile 2"}}"#,
        )
        .unwrap();
        std::fs::write(data.join("Default/Preferences"), "{}").unwrap();
        std::fs::write(data.join("Default/Cache/data_0"), "cached").unwrap();
        let cookies = rusqlite::Connection::open(data.join("Default/Cookies")).unwrap();
        cookies
            .execute_batch("CREATE TABLE cookies(name TEXT); INSERT INTO cookies VALUES ('sid');")
            .unwrap();
        Browser {
            name: "chrome".to_owned(),
            executable: PathBuf::from("/nonexistent/chrome"),
            user_data_dir: data,
        }
    }

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn the_pinned_profile_is_copied_with_its_databases_and_without_caches() {
        let root = temp();
        let browser = fake_browser(root.path());
        let cache = root.path().join("cache/browser");
        let config = BrowserConfig {
            use_real_profile: true,
            real_profile_pin: "Default".to_owned(),
            ..BrowserConfig::default()
        };
        let Choice::Real(_, snapshot) = prepare(&config, &granted(), &Fixed(Ok(browser)), &cache)
        else {
            panic!("no snapshot");
        };
        assert_eq!(snapshot.profile, "Default");
        assert_eq!(snapshot.user_data_dir, cache.join("real-profile/chrome"));
        let copied = snapshot.user_data_dir.join("Default");
        assert!(copied.join("Preferences").is_file());
        assert!(!copied.join("Cache").exists());
        let db = rusqlite::Connection::open(copied.join("Cookies")).unwrap();
        let name: String = db
            .query_row("SELECT name FROM cookies", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "sid");
        assert!(snapshot.user_data_dir.join("Local State").is_file());
    }

    #[test]
    fn without_a_pin_the_last_used_profile_is_copied() {
        let root = temp();
        let browser = fake_browser(root.path());
        let config = BrowserConfig {
            use_real_profile: true,
            ..BrowserConfig::default()
        };
        let cache = root.path().join("cache/browser");
        let Choice::Real(_, snapshot) = prepare(&config, &granted(), &Fixed(Ok(browser)), &cache)
        else {
            panic!("no snapshot");
        };
        assert_eq!(snapshot.profile, "Profile 2");
    }

    /// Scenario: Missing pin.
    #[test]
    fn a_missing_pin_is_unavailable_naming_the_pin() {
        let root = temp();
        let browser = fake_browser(root.path());
        let config = BrowserConfig {
            use_real_profile: true,
            real_profile_pin: "Profile 9".to_owned(),
            ..BrowserConfig::default()
        };
        let cache = root.path().join("cache/browser");
        let Choice::Unavailable(why) = prepare(&config, &granted(), &Fixed(Ok(browser)), &cache)
        else {
            panic!("expected unavailable");
        };
        assert!(why.contains("'Profile 9'"), "{why}");
        assert!(!snapshot::root(&cache).join("chrome").exists());
    }

    /// Scenario: Revoking consent.
    #[test]
    fn turning_the_setting_off_deletes_the_snapshot() {
        let root = temp();
        let browser = fake_browser(root.path());
        let cache = root.path().join("cache/browser");
        let mut config = BrowserConfig {
            use_real_profile: true,
            ..BrowserConfig::default()
        };
        let detect = Fixed(Ok(browser));
        assert!(matches!(
            prepare(&config, &granted(), &detect, &cache),
            Choice::Real(..)
        ));
        assert!(snapshot::root(&cache).join("chrome").is_dir());
        config.use_real_profile = false;
        assert_eq!(
            prepare(&config, &granted(), &detect, &cache),
            Choice::Throwaway
        );
        assert!(!snapshot::root(&cache).exists());
    }

    #[test]
    fn without_the_grant_nothing_is_copied() {
        let root = temp();
        let browser = fake_browser(root.path());
        let cache = root.path().join("cache/browser");
        let config = BrowserConfig {
            use_real_profile: true,
            ..BrowserConfig::default()
        };
        let none = CapabilitySet::default();
        assert_eq!(
            prepare(&config, &none, &Fixed(Ok(browser)), &cache),
            Choice::Throwaway
        );
        assert!(!snapshot::root(&cache).exists());
    }

    #[test]
    fn a_database_the_browser_holds_locked_makes_the_copy_impossible() {
        let root = temp();
        let browser = fake_browser(root.path());
        let held =
            rusqlite::Connection::open(browser.user_data_dir.join("Profile 2/History")).unwrap();
        held.execute_batch("PRAGMA locking_mode=EXCLUSIVE; CREATE TABLE urls(u TEXT); INSERT INTO urls VALUES ('x');")
            .unwrap();
        let config = BrowserConfig {
            use_real_profile: true,
            ..BrowserConfig::default()
        };
        let cache = root.path().join("cache/browser");
        let Choice::Unavailable(why) = prepare(&config, &granted(), &Fixed(Ok(browser)), &cache)
        else {
            panic!("expected unavailable");
        };
        assert!(why.contains("locked"), "{why}");
        drop(held);
    }
}

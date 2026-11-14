//! Version skew between the attached daemon and the bundled binary (spec:
//! desktop-app, Version skew restarts a stale daemon). A different build
//! is restarted (after asking when turns are running), except a daemon
//! newer than the app: that one is never downgraded.

use semver::Version;

/// How the attached daemon relates to the bundled binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skew {
    /// The same build.
    Same,
    /// The daemon is newer than the app: keep it, ask for an app update.
    DaemonNewer {
        /// The daemon's version.
        daemon: String,
        /// The bundled binary's version.
        bundled: String,
    },
    /// Another build no newer than the app: restart it.
    Stale,
}

/// Compares the daemon's `(version, build_id)` with the bundled binary's.
#[must_use]
pub fn compare(daemon: (&str, &str), bundled: (&str, &str)) -> Skew {
    if daemon.1 == bundled.1 {
        return Skew::Same;
    }
    match (Version::parse(daemon.0), Version::parse(bundled.0)) {
        (Ok(d), Ok(b)) if d > b => Skew::DaemonNewer {
            daemon: daemon.0.to_owned(),
            bundled: bundled.0.to_owned(),
        },
        // An unreadable daemon version is stale: the bundled binary is
        // the authority on what this app speaks.
        _ => Skew::Stale,
    }
}

/// The notice shown instead of a downgrade.
#[must_use]
pub fn update_notice(daemon: &str, bundled: &str) -> String {
    format!("Prometheus {daemon} is running but this app bundles {bundled}; update the app")
}

#[cfg(test)]
mod tests {
    use super::{Skew, compare, update_notice};

    #[test]
    fn a_daemon_that_outlived_a_cli_update_to_an_older_app_is_kept() {
        let skew = compare(("1.6.0", "new"), ("1.5.2", "old"));
        assert_eq!(
            skew,
            Skew::DaemonNewer {
                daemon: "1.6.0".into(),
                bundled: "1.5.2".into()
            }
        );
        assert_eq!(
            update_notice("1.6.0", "1.5.2"),
            "Prometheus 1.6.0 is running but this app bundles 1.5.2; update the app"
        );
    }

    #[test]
    fn other_builds_are_stale_and_the_same_build_is_kept() {
        assert_eq!(compare(("1.5.2", "a"), ("1.6.0", "b")), Skew::Stale);
        assert_eq!(compare(("1.6.0", "a"), ("1.6.0", "b")), Skew::Stale);
        assert_eq!(compare(("garbage", "a"), ("1.6.0", "b")), Skew::Stale);
        assert_eq!(compare(("9.9.9", "same"), ("1.0.0", "same")), Skew::Same);
    }
}

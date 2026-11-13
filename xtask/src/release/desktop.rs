//! The desktop packages of a release (spec: distribution-and-update,
//! Desktop packages and signing; desktop-app, Signed in-app updates):
//! `desktop-feed` writes the in-app updater's channel feed over the
//! updater archives present, and `verify` checks the desktop packages
//! present per OS. An OS whose notarization or Authenticode/MSIX signing
//! failed publishes no desktop package at all, so verification accepts a
//! missing OS but never a partial one, and every package and the feed must
//! carry a valid minisign signature.

use std::collections::BTreeMap;
use std::path::Path;

use pmt_desktop::update::{FEED_SCHEMA, Feed};
use pmt_update::{Channel, ReleaseVersion, TrustRoot};

/// The packages of one OS: published together or not at all.
pub(super) struct Group {
    /// The OS.
    pub(super) os: &'static str,
    /// Its files.
    pub(super) files: &'static [&'static str],
    /// The in-app updater archive and its feed platform key, if the
    /// desktop updater updates these installs.
    pub(super) updater: Option<(&'static str, &'static str)>,
}

/// Every desktop package a release can carry.
pub(super) const GROUPS: &[Group] = &[
    Group {
        os: "macos",
        files: &[
            "prometheus-desktop-universal2-apple-darwin.dmg",
            "prometheus-desktop-universal2-apple-darwin.app.tar.gz",
        ],
        updater: Some((
            "prometheus-desktop-universal2-apple-darwin.app.tar.gz",
            "darwin-universal",
        )),
    },
    Group {
        os: "linux-x86_64",
        files: &[
            "prometheus-desktop-x86_64-linux.AppImage",
            "prometheus-desktop-x86_64-linux.deb",
            "prometheus-desktop-x86_64-linux.rpm",
        ],
        updater: Some(("prometheus-desktop-x86_64-linux.AppImage", "linux-x86_64")),
    },
    Group {
        os: "linux-aarch64",
        files: &[
            "prometheus-desktop-aarch64-linux.AppImage",
            "prometheus-desktop-aarch64-linux.deb",
            "prometheus-desktop-aarch64-linux.rpm",
        ],
        updater: Some(("prometheus-desktop-aarch64-linux.AppImage", "linux-aarch64")),
    },
    Group {
        os: "windows",
        files: &[
            "prometheus-desktop-x86_64-pc-windows-msvc.msixbundle",
            "prometheus-desktop.appinstaller",
        ],
        updater: None,
    },
];

/// The feed file of `channel`.
pub(super) fn feed_name(channel: Channel) -> String {
    format!("desktop-{channel}.json")
}

/// Writes `desktop-<channel>.json` into `dir` for the updater archives it
/// holds.
pub(super) fn write_feed(dir: &Path, tag: &str) -> Result<(), String> {
    let version = ReleaseVersion::parse(tag).map_err(|e| e.to_string())?;
    let platforms: BTreeMap<String, String> = GROUPS
        .iter()
        .filter_map(|g| g.updater)
        .filter(|(file, _)| dir.join(file).is_file())
        .map(|(file, key)| (key.to_owned(), file.to_owned()))
        .collect();
    let feed = Feed {
        schema: FEED_SCHEMA.to_owned(),
        version: version.to_string().trim_start_matches('v').to_owned(),
        notes: format!("Prometheus {version}"),
        platforms,
    };
    let path = dir.join(feed_name(version.channel()));
    let text = serde_json::to_string_pretty(&feed).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

fn signed_ok(dir: &Path, name: &str, trust: &TrustRoot) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(dir.join(name)).map_err(|_| format!("{name}: missing"))?;
    let signature = std::fs::read_to_string(dir.join(format!("{name}.minisig")))
        .map_err(|_| format!("{name}.minisig: missing"))?;
    trust
        .verify(name, &bytes, &signature)
        .map_err(|e| e.to_string())?;
    Ok(bytes)
}

/// Checks the signed feed lists exactly `updaters` at `version`.
fn check_feed(
    dir: &Path,
    version: &ReleaseVersion,
    trust: &TrustRoot,
    updaters: &BTreeMap<String, String>,
    problems: &mut Vec<String>,
) {
    let name = feed_name(version.channel());
    let feed = signed_ok(dir, &name, trust).and_then(|bytes| {
        serde_json::from_slice::<Feed>(&bytes).map_err(|e| format!("{name}: {e}"))
    });
    match feed {
        Ok(feed) if &feed.platforms != updaters => problems.push(format!(
            "{name}: lists {:?}, the release holds {updaters:?}",
            feed.platforms
        )),
        Ok(feed) if feed.version != version.to_string().trim_start_matches('v') => {
            problems.push(format!("{name}: version {} is not {version}", feed.version));
        }
        Ok(_) => {}
        Err(problem) => problems.push(problem),
    }
}

/// The problems of the desktop packages in `dir` of release `version`.
pub(super) fn verify(dir: &Path, version: &ReleaseVersion, trust: &TrustRoot) -> Vec<String> {
    let mut problems = Vec::new();
    let mut updaters = BTreeMap::new();
    for group in GROUPS {
        let present: Vec<&str> = group
            .files
            .iter()
            .copied()
            .filter(|f| dir.join(f).is_file())
            .collect();
        if present.is_empty() {
            continue;
        }
        for file in group.files {
            if let Err(problem) = signed_ok(dir, file, trust) {
                problems.push(format!("desktop {}: {problem}", group.os));
            }
        }
        if let Some((file, key)) = group.updater {
            updaters.insert(key.to_owned(), file.to_owned());
        }
    }
    if !updaters.is_empty() {
        check_feed(dir, version, trust, &updaters, &mut problems);
    }
    problems
}

#[cfg(test)]
mod tests {
    use pmt_update::{ReleaseVersion, TrustRoot};

    use super::{verify, write_feed};

    /// A fresh directory under `target/xtask-test/`.
    fn fresh(name: &str) -> std::path::PathBuf {
        let dir = crate::repo_root().join("target/xtask-test").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn signer(dir: &std::path::Path) -> (impl Fn(&str), TrustRoot) {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let pub_text = pair.pk.to_box().unwrap().into_string();
        let trust =
            TrustRoot::from_keys(&[("release", Box::leak(pub_text.into_boxed_str()))]).unwrap();
        let dir = dir.to_owned();
        let sign = move |name: &str| {
            let data = std::fs::read(dir.join(name)).unwrap();
            let sig = minisign::sign(None, &pair.sk, &data[..], None, None).unwrap();
            std::fs::write(dir.join(format!("{name}.minisig")), sig.into_string()).unwrap();
        };
        (sign, trust)
    }

    #[test]
    fn a_signed_os_group_with_its_feed_verifies_and_a_partial_one_does_not() {
        let dir = fresh("desktop-groups");
        let (sign, trust) = signer(&dir);
        let version = ReleaseVersion::parse("v1.6.0").unwrap();
        for file in [
            "prometheus-desktop-universal2-apple-darwin.dmg",
            "prometheus-desktop-universal2-apple-darwin.app.tar.gz",
        ] {
            std::fs::write(dir.join(file), file).unwrap();
            sign(file);
        }
        write_feed(&dir, "v1.6.0").unwrap();
        sign("desktop-stable.json");
        assert_eq!(verify(&dir, &version, &trust), Vec::<String>::new());
        // Linux published only its AppImage: refused.
        std::fs::write(dir.join("prometheus-desktop-x86_64-linux.AppImage"), "x").unwrap();
        sign("prometheus-desktop-x86_64-linux.AppImage");
        let problems = verify(&dir, &version, &trust);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("prometheus-desktop-x86_64-linux.deb: missing")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("desktop-stable.json: lists")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_tampered_package_fails_verification() {
        let dir = fresh("desktop-tampered");
        let (sign, trust) = signer(&dir);
        let version = ReleaseVersion::parse("v1.6.0").unwrap();
        for file in [
            "prometheus-desktop-x86_64-pc-windows-msvc.msixbundle",
            "prometheus-desktop.appinstaller",
        ] {
            std::fs::write(dir.join(file), file).unwrap();
            sign(file);
        }
        std::fs::write(dir.join("prometheus-desktop.appinstaller"), "changed").unwrap();
        let problems = verify(&dir, &version, &trust);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("signature verification failed"),
            "{problems:?}"
        );
    }
}

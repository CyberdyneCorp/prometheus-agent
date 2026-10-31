//! `cargo xtask deps-age`: the dependency release-age gate (spec:
//! distribution-and-update, Dependency release-age gate).
//!
//! Every crate version that `Cargo.lock` gained since the base commit must
//! have been published at least `release_age_days` (`deps-policy.toml`)
//! before the lock change that added it, unless `release_age_exempt` names
//! the crate (certificate stores, so trust-root updates are not delayed) or
//! an unexpired `[[waiver]]` names the version. An expired waiver fails.
//!
//! Publish times come from the crates.io sparse index (`pubtime`). The lock
//! change time of a version is the commit time of the first commit since
//! the base whose `Cargo.lock` holds it, or now for an uncommitted change.
//!
//! The workspace has no JavaScript or Python lockfiles yet; the dashboard,
//! desktop and Python-bridge packages join this gate with their lockfiles.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use jiff::{Timestamp, ToSpan as _};
use serde::Deserialize;

const POLICY_FILE: &str = "deps-policy.toml";
const INDEX: &str = "https://index.crates.io";
const CRATES_IO_SOURCES: &[&str] = &[
    "registry+https://github.com/rust-lang/crates.io-index",
    "sparse+https://index.crates.io/",
];

/// `deps-policy.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Policy {
    /// Minimum age, in days, of a newly locked version.
    pub(crate) release_age_days: i64,
    /// Crates exempt from the gate.
    #[serde(default)]
    pub(crate) release_age_exempt: Vec<String>,
    /// Accepted young versions.
    #[serde(default, rename = "waiver")]
    pub(crate) waivers: Vec<Waiver>,
}

/// One `[[waiver]]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Waiver {
    /// Crate name.
    pub(crate) package: String,
    /// Exact version.
    pub(crate) version: String,
    /// Why it cannot wait.
    pub(crate) reason: String,
    /// Last day (UTC, `YYYY-MM-DD`) it applies.
    pub(crate) expires: String,
}

impl Waiver {
    /// Whether it still applies on `today` (an unparsable date never does).
    fn applies(&self, today: jiff::civil::Date) -> bool {
        self.expires
            .parse::<jiff::civil::Date>()
            .is_ok_and(|expires| expires >= today)
    }
}

/// A version the lockfile gained.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Locked {
    /// Crate name.
    pub(crate) name: String,
    /// Version.
    pub(crate) version: String,
    /// When it was locked.
    pub(crate) at: Timestamp,
}

/// Runs the gate for the repository at `root` against commit `base`.
///
/// # Errors
///
/// Every violation, one per line.
pub(crate) fn run(root: &Path, base: &str) -> Result<(), String> {
    let policy = load_policy(root)?;
    let added = added_since(root, base)?;
    if added.is_empty() {
        println!("deps-age: no crate versions added since {base}");
        return Ok(());
    }
    let lookup = publish_times(&added)?;
    let violations = check(
        &added,
        &policy,
        &|name, version| {
            lookup
                .get(&(name.to_owned(), version.to_owned()))
                .copied()
                .ok_or_else(|| format!("{name} {version} is not in the crates.io index"))
        },
        Timestamp::now(),
    );
    if violations.is_empty() {
        println!(
            "deps-age: {} newly locked crate version(s) are at least {} days old",
            added.len(),
            policy.release_age_days
        );
        return Ok(());
    }
    for v in &violations {
        eprintln!("deps-age: {v}");
    }
    Err(format!(
        "{} dependency release-age violation(s)",
        violations.len()
    ))
}

/// The violations among `added`; `published` answers a version's publish
/// time.
#[must_use]
pub(crate) fn check(
    added: &[Locked],
    policy: &Policy,
    published: &dyn Fn(&str, &str) -> Result<Timestamp, String>,
    now: Timestamp,
) -> Vec<String> {
    let today = now.to_zoned(jiff::tz::TimeZone::UTC).date();
    let mut violations = Vec::new();
    for waiver in &policy.waivers {
        if !waiver.applies(today) {
            violations.push(format!(
                "the waiver for {} {} expired on {} ({}); renew or remove it in {POLICY_FILE}",
                waiver.package, waiver.version, waiver.expires, waiver.reason
            ));
        }
    }
    for locked in added {
        if policy.release_age_exempt.contains(&locked.name) {
            continue;
        }
        let waived = policy
            .waivers
            .iter()
            .any(|w| w.package == locked.name && w.version == locked.version && w.applies(today));
        if waived {
            continue;
        }
        match published(&locked.name, &locked.version) {
            Ok(at) => {
                let ripe = at + (policy.release_age_days * 24).hours();
                if ripe > locked.at {
                    violations.push(format!(
                        "crate {} {} was published at {at}, less than the {}-day minimum before it was locked at {}; wait, or add a [[waiver]] with a reason and expiry to {POLICY_FILE}",
                        locked.name, locked.version, policy.release_age_days, locked.at
                    ));
                }
            }
            Err(e) => violations.push(e),
        }
    }
    violations
}

fn load_policy(root: &Path) -> Result<Policy, String> {
    let path = root.join(POLICY_FILE);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{POLICY_FILE}: {e}"))?;
    toml::from_str(&text).map_err(|e| format!("{POLICY_FILE}: {e}"))
}

#[derive(Deserialize)]
struct Lockfile {
    #[serde(default)]
    package: Vec<LockedPackage>,
}

#[derive(Deserialize)]
struct LockedPackage {
    name: String,
    version: String,
    source: Option<String>,
}

/// The crates.io versions of a `Cargo.lock` text.
fn crates_io_versions(text: &str) -> Result<BTreeSet<(String, String)>, String> {
    let lock: Lockfile = toml::from_str(text).map_err(|e| format!("Cargo.lock: {e}"))?;
    Ok(lock
        .package
        .into_iter()
        .filter(|p| {
            p.source
                .as_deref()
                .is_some_and(|s| CRATES_IO_SOURCES.contains(&s))
        })
        .map(|p| (p.name, p.version))
        .collect())
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The versions the working tree's lock gained since `base`, each with
/// the time it was first locked on this branch.
fn added_since(root: &Path, base: &str) -> Result<Vec<Locked>, String> {
    let before = crates_io_versions(&git(root, &["show", &format!("{base}:Cargo.lock")])?)?;
    let now = crates_io_versions(
        &std::fs::read_to_string(root.join("Cargo.lock"))
            .map_err(|e| format!("Cargo.lock: {e}"))?,
    )?;
    let mut pending: BTreeSet<(String, String)> = now.difference(&before).cloned().collect();
    let mut first_locked: BTreeMap<(String, String), Timestamp> = BTreeMap::new();
    let log = git(
        root,
        &[
            "log",
            "--reverse",
            "--format=%H %ct",
            &format!("{base}..HEAD"),
            "--",
            "Cargo.lock",
        ],
    )?;
    for line in log.lines() {
        if pending.is_empty() {
            break;
        }
        let Some((commit, time)) = line.split_once(' ') else {
            continue;
        };
        let at = time
            .parse::<i64>()
            .ok()
            .and_then(|s| Timestamp::from_second(s).ok())
            .ok_or_else(|| format!("bad commit time in: {line}"))?;
        let locked = crates_io_versions(&git(root, &["show", &format!("{commit}:Cargo.lock")])?)?;
        pending.retain(|key| {
            if locked.contains(key) {
                first_locked.insert(key.clone(), at);
                false
            } else {
                true
            }
        });
    }
    let uncommitted = Timestamp::now();
    let mut added: Vec<Locked> = now
        .difference(&before)
        .map(|(name, version)| Locked {
            name: name.clone(),
            version: version.clone(),
            at: first_locked
                .get(&(name.clone(), version.clone()))
                .copied()
                .unwrap_or(uncommitted),
        })
        .collect();
    added.sort();
    Ok(added)
}

/// The sparse-index path of a crate.
fn index_path(name: &str) -> String {
    let name = name.to_ascii_lowercase();
    match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", &name[..1]),
        _ => format!("{}/{}/{name}", &name[..2], &name[2..4]),
    }
}

#[derive(Deserialize)]
struct IndexLine {
    vers: String,
    pubtime: Option<String>,
}

/// Publish times of every version in one index file.
fn parse_index(text: &str) -> BTreeMap<String, Timestamp> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<IndexLine>(line).ok())
        .filter_map(|l| Some((l.vers, l.pubtime?.parse().ok()?)))
        .collect()
}

/// Fetches the publish time of every added version from the sparse index.
fn publish_times(added: &[Locked]) -> Result<BTreeMap<(String, String), Timestamp>, String> {
    let names: BTreeSet<&str> = added.iter().map(|l| l.name.as_str()).collect();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let client = pmt_net::client(&pmt_net::OutboundPolicy::default()).map_err(|e| e.to_string())?;
    let files: Vec<(String, Result<String, String>)> = runtime.block_on(async {
        let mut set = tokio::task::JoinSet::new();
        let limit = std::sync::Arc::new(tokio::sync::Semaphore::new(16));
        for name in names {
            let (client, limit, name) = (client.clone(), limit.clone(), name.to_owned());
            set.spawn(async move {
                let _permit = limit.acquire().await;
                let url = format!("{INDEX}/{}", index_path(&name));
                let body = async {
                    let response = client.get(&url).send().await.map_err(|e| e.to_string())?;
                    if !response.status().is_success() {
                        return Err(format!("{url}: HTTP {}", response.status()));
                    }
                    response.text().await.map_err(|e| e.to_string())
                };
                (name, body.await)
            });
        }
        let mut files = Vec::new();
        while let Some(joined) = set.join_next().await {
            files.push(joined.map_err(|e| e.to_string())?);
        }
        Ok::<_, String>(files)
    })?;
    let mut times = BTreeMap::new();
    for (name, body) in files {
        let body = body.map_err(|e| {
            format!("cannot read the crates.io index for {name} (the release-age gate needs the network): {e}")
        })?;
        for (version, at) in parse_index(&body) {
            times.insert((name.clone(), version), at);
        }
    }
    Ok(times)
}

#[cfg(test)]
mod tests {
    use jiff::{Timestamp, ToSpan as _};

    use super::{Locked, Policy, Waiver, check, index_path, parse_index};

    fn policy(waivers: Vec<Waiver>) -> Policy {
        Policy {
            release_age_days: 3,
            release_age_exempt: vec!["webpki-root-certs".to_owned()],
            waivers,
        }
    }

    fn locked(name: &str, at: Timestamp) -> Locked {
        Locked {
            name: name.to_owned(),
            version: "1.2.3".to_owned(),
            at,
        }
    }

    #[test]
    fn a_day_old_crate_is_rejected_with_its_publish_time() {
        let now: Timestamp = "2026-10-31T12:00:00Z".parse().unwrap();
        let published: Timestamp = "2026-10-30T16:00:00Z".parse().unwrap();
        let v = check(
            &[locked("fresh", now)],
            &policy(Vec::new()),
            &|_, _| Ok(published),
            now,
        );
        assert_eq!(v.len(), 1);
        assert!(
            v[0].starts_with(
                "crate fresh 1.2.3 was published at 2026-10-30T16:00:00Z, less than the 3-day minimum"
            ),
            "{}",
            v[0]
        );
    }

    #[test]
    fn old_enough_exempt_and_waived_versions_pass() {
        let now: Timestamp = "2026-10-31T12:00:00Z".parse().unwrap();
        let young = now - 20.hours();
        let waiver = Waiver {
            package: "waived".to_owned(),
            version: "1.2.3".to_owned(),
            reason: "security fix".to_owned(),
            expires: "2026-11-30".to_owned(),
        };
        let v = check(
            &[
                locked("webpki-root-certs", now),
                locked("waived", now),
                locked("seasoned", now),
            ],
            &policy(vec![waiver]),
            &|name, _| {
                Ok(if name == "seasoned" {
                    now - 96.hours()
                } else {
                    young
                })
            },
            now,
        );
        assert!(v.is_empty(), "{v:?}");
    }

    #[test]
    fn age_is_measured_at_the_lock_change() {
        let now: Timestamp = "2026-10-31T12:00:00Z".parse().unwrap();
        // Locked a day after publication, a week ago: still a violation.
        let published = now - (8 * 24).hours();
        let v = check(
            &[locked("early", published + 24.hours())],
            &policy(Vec::new()),
            &|_, _| Ok(published),
            now,
        );
        assert_eq!(v.len(), 1, "{v:?}");
    }

    #[test]
    fn an_expired_waiver_fails() {
        let now: Timestamp = "2026-10-31T12:00:00Z".parse().unwrap();
        let waiver = Waiver {
            package: "old".to_owned(),
            version: "1.0.0".to_owned(),
            reason: "temporary".to_owned(),
            expires: "2026-10-01".to_owned(),
        };
        let v = check(&[], &policy(vec![waiver]), &|_, _| Ok(now), now);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("expired on 2026-10-01"), "{}", v[0]);
    }

    #[test]
    fn sparse_index_paths_and_lines() {
        assert_eq!(index_path("a"), "1/a");
        assert_eq!(index_path("io"), "2/io");
        assert_eq!(index_path("zip"), "3/z/zip");
        assert_eq!(index_path("Serde"), "se/rd/serde");
        let times = parse_index(
            "{\"name\":\"zip\",\"vers\":\"9.0.5\",\"pubtime\":\"2026-10-17T18:45:16Z\"}\n{\"name\":\"zip\",\"vers\":\"0.1.0\"}\n",
        );
        assert_eq!(times.len(), 1);
        assert_eq!(times["9.0.5"].to_string(), "2026-10-17T18:45:16Z");
    }
}

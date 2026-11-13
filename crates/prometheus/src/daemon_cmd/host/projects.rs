//! The Projects sidebar (spec: desktop-app, Repository discovery for the
//! Projects sidebar; projects, Repository discovery): explicit projects,
//! the repositories of the profile's sessions and the repositories the
//! background scan found, each repository offered once and only while no
//! project owns it.
//!
//! The scan runs in the background when a client that sees discovery
//! asks and the cache holds nothing for the current scan keys (or is a
//! day old); its rows are cached in the profile's store with the
//! fingerprint of those keys, so changing any key invalidates only this
//! profile's cache, and turning scanning off clears it. The scan skips
//! what the profile's sandbox may not read (its credential stores and the
//! Prometheus home below a scan root).
//!
//! Only the desktop scans by default; another client (the dashboard)
//! reads the same cache and starts a scan only when the profile turns
//! scanning on explicitly.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Instant;

use jiff::Timestamp;
use pmt_config::ProfileScope;
use pmt_config::grants::{GrantsFile, Subject, baseline};
use pmt_config::schema::ProjectsConfig;
use pmt_core::error::Error;
use pmt_rpc::contracts::{ProjectRow, ProjectsList, RepoRow};
use pmt_store::{NewProject, Project, Store};
use pmt_worktrees::discover::Scan;

use super::Profile;

/// A cache older than this is refreshed in the background.
const REFRESH_AFTER_SECS: i64 = 24 * 60 * 60;
/// The sessions whose folders are looked at for repositories.
const SESSIONS_LOOKED_AT: u32 = 500;

/// The profiles whose scan is running.
static SCANNING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

/// When each profile was last scanned, with which keys: a scan that
/// found nothing is not repeated on every listing.
static SCANNED: LazyLock<Mutex<HashMap<String, (String, Instant)>>> = LazyLock::new(Mutex::default);

fn scanning() -> std::sync::MutexGuard<'static, HashSet<String>> {
    SCANNING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The fingerprint of the scan keys.
fn settings(config: &ProjectsConfig) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (
        &config.repo_scan_roots,
        config.repo_scan_max_depth,
        &config.repo_scan_exclude_paths,
    )
        .hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn row(project: &Project) -> ProjectRow {
    ProjectRow {
        id: project.id.to_string(),
        name: project.name.clone(),
        slug: project.slug.clone(),
        path: project.primary_path().map(str::to_owned),
    }
}

fn repo_row(path: &Path, last_seen: Option<Timestamp>) -> RepoRow {
    RepoRow {
        path: path.display().to_string(),
        name: path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
        last_seen: last_seen.map(|t| t.to_string()),
    }
}

/// The repository containing `dir` (its nearest folder holding `.git`).
fn repository_of(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|d| d.join(".git").exists())
        .map(Path::to_path_buf)
}

/// The repositories of the profile's recent sessions.
fn inferred(store: &Store) -> Result<BTreeSet<PathBuf>, Error> {
    let options = pmt_store::listing::ListOptions {
        limit: Some(SESSIONS_LOOKED_AT),
        all: true,
        ..Default::default()
    };
    Ok(store
        .list_sessions(&options)?
        .iter()
        .filter_map(|s| s.cwd.as_deref())
        .filter_map(|cwd| repository_of(Path::new(cwd)))
        .collect())
}

/// The subtrees below `roots` the profile's sandbox denies reading: the
/// sandbox policy of a session working in each root (the profile's
/// baseline and grants), as the session's file tools would apply it.
fn sandbox_denied(
    scope: &ProfileScope,
    home: &Path,
    roots: &[PathBuf],
) -> Result<Vec<PathBuf>, Error> {
    let granted = GrantsFile::of(scope.profile())
        .load()?
        .capabilities(&Subject::Profile);
    let capabilities = baseline(&scope.config().security).union(&granted);
    Ok(denied_below(&capabilities, home, scope.home(), roots))
}

fn denied_below(
    capabilities: &pmt_core::capability::CapabilitySet,
    home: &Path,
    prometheus_home: &Path,
    roots: &[PathBuf],
) -> Vec<PathBuf> {
    let mut denied: Vec<PathBuf> = roots
        .iter()
        .flat_map(|root| {
            let paths = pmt_sandbox::Paths {
                user_home: home.to_path_buf(),
                prometheus_home: prometheus_home.to_path_buf(),
                workspace: root.clone(),
                scratch: Vec::new(),
            };
            pmt_sandbox::Policy::from_set(capabilities, &paths)
                .denied()
                .into_iter()
                .map(Path::to_path_buf)
                .collect::<Vec<_>>()
        })
        .collect();
    denied.sort();
    denied.dedup();
    denied
}

/// Scans in the background and caches what it found.
fn start_scan(profile: &str, store: Arc<Store>, scope: &ProfileScope, home: &Path) {
    let config = &scope.config().projects;
    let fingerprint = settings(config);
    {
        let mut scanned = SCANNED.lock().unwrap_or_else(PoisonError::into_inner);
        let recent = scanned.get(profile).is_some_and(|(keys, at)| {
            *keys == fingerprint && at.elapsed().as_secs() < REFRESH_AFTER_SECS.unsigned_abs()
        });
        if recent || !scanning().insert(profile.to_owned()) {
            return;
        }
        scanned.insert(profile.to_owned(), (fingerprint.clone(), Instant::now()));
    }
    let scan = Scan::from_keys(
        &config.repo_scan_roots,
        config.repo_scan_max_depth,
        &config.repo_scan_exclude_paths,
        home,
    );
    let scan = match sandbox_denied(scope, home, &scan.roots) {
        Ok(denied) => scan.denying(denied),
        Err(err) => {
            tracing::warn!(profile, "the repository scan did not start: {err}");
            scanning().remove(profile);
            return;
        }
    };
    let profile = profile.to_owned();
    drop(pmt_core::spawn::blocking(move || {
        let found: Vec<(String, String)> = scan
            .run()
            .iter()
            .map(|p| {
                let row = repo_row(p, None);
                (row.path, row.name)
            })
            .collect();
        if let Err(err) = store.replace_discovered_repos(&fingerprint, &found, Timestamp::now()) {
            tracing::warn!(profile, "the repository scan was not cached: {err}");
        }
        scanning().remove(&profile);
    }));
}

/// The discovered rows for the profile's scan keys, starting a scan when
/// `scans` and the cache is due; the cache is cleared when scanning is
/// turned off.
fn discovered(
    profile: &str,
    store: &Arc<Store>,
    scope: &ProfileScope,
    scans: bool,
    home: Option<PathBuf>,
) -> Result<Vec<RepoRow>, Error> {
    let config = &scope.config().projects;
    if config.repo_scan_enabled == Some(false) {
        store.clear_discovered_repos()?;
        return Ok(Vec::new());
    }
    let cached = store.discovered_repos(&settings(config))?;
    let stale =
        cached.iter().map(|r| r.last_seen).min().is_none_or(|seen| {
            Timestamp::now().as_second() - seen.as_second() > REFRESH_AFTER_SECS
        });
    if scans
        && stale
        && let Some(home) = home
    {
        start_scan(profile, Arc::clone(store), scope, &home);
    }
    Ok(cached
        .iter()
        .map(|r| repo_row(Path::new(&r.path), Some(r.last_seen)))
        .collect())
}

/// The sidebar's rows of `profile`; `desktop`: the asking client scans
/// unless scanning is turned off (others only when it is turned on).
pub(super) fn list(
    name: &str,
    profile: &Profile,
    desktop: bool,
    home: Option<PathBuf>,
) -> Result<ProjectsList, Error> {
    let store = &profile.store;
    let scope = profile.scope();
    let scans = scope.config().projects.scans_for(desktop);
    let projects = store.projects(false)?;
    let owned = |path: &Path| {
        projects
            .iter()
            .any(|p| p.folders.iter().any(|f| Path::new(&f.path) == path))
    };
    let inferred: Vec<RepoRow> = inferred(store)?
        .iter()
        .filter(|p| !owned(p))
        .map(|p| repo_row(p, None))
        .collect();
    let discovered = discovered(name, store, &scope, scans, home)?
        .into_iter()
        .filter(|r| !owned(Path::new(&r.path)) && !inferred.iter().any(|i| i.path == r.path))
        .collect();
    Ok(ProjectsList {
        projects: projects.iter().map(row).collect(),
        inferred,
        discovered,
        scanning: scanning().contains(name),
    })
}

/// Creates a project of the repository at `path`.
pub(super) fn create(
    profile: &Profile,
    path: &str,
    name: Option<&str>,
) -> Result<ProjectRow, Error> {
    let folder = PathBuf::from(path);
    let name = name.map_or_else(|| repo_row(&folder, None).name, str::to_owned);
    let new = NewProject {
        name,
        folders: vec![folder],
        ..NewProject::default()
    };
    Ok(row(&profile.store.create_project(&new, false)?))
}

#[cfg(test)]
mod tests {
    use pmt_config::grants::baseline;
    use pmt_worktrees::discover::Scan;

    use super::denied_below;

    fn repo(at: &std::path::Path) {
        std::fs::create_dir_all(at.join(".git")).unwrap();
    }

    /// Scenario support (projects, Repository discovery): a Prometheus
    /// home below the scanned home is what the sandbox denies, so a
    /// repository inside it is never discovered.
    #[test]
    fn the_scan_skips_what_the_profile_sandbox_denies() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("user");
        let prometheus = home.join("prometheus-home");
        repo(&home.join("code/app"));
        repo(&prometheus.join("profiles/work/workspace/repo"));
        let capabilities = baseline(&pmt_config::schema::SecurityConfig::default());
        let scan = Scan::from_keys(&[], 4, &[], &home);
        let denied = denied_below(&capabilities, &home, &prometheus, &scan.roots);
        assert!(
            denied.contains(&prometheus.canonicalize().unwrap()),
            "{denied:?}"
        );
        assert_eq!(scan.denying(denied).run(), vec![home.join("code/app")]);
    }
}

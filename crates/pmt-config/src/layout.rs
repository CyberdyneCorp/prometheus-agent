//! The home and profile layout of design D13 (spec: configuration-and-
//! profiles, Home and profile layout). The home root holds only
//! machine-wide state; every profile, `default` included, is a directory
//! under `profiles/<name>/`. Directories are 0700 and private files 0600
//! (on POSIX; Windows ACLs belong to the security group), and looser modes
//! found on an existing home are tightened with a WARN.

use std::io;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;

use crate::write::ConfigFile;

/// The profile used when none is selected.
pub const DEFAULT_PROFILE: &str = "default";
/// `<home>/active_profile`: the name of the profile used without `-p`.
pub const ACTIVE_PROFILE_FILE: &str = "active_profile";
/// `<home>/profiles/.deleted/<name>`: tombstones of deleted profiles.
pub const TOMBSTONE_DIR: &str = ".deleted";
/// Names a new profile may not take.
pub const RESERVED_NAMES: &[&str] = &[
    "prometheus",
    "pmt",
    "test",
    "tmp",
    "root",
    "all",
    "none",
    "run",
    "logs",
    "tools",
];
/// Directories of the home root (beside `active_profile` and `daemon.toml`).
pub const ROOT_DIRS: &[&str] = &["run", "logs", "runtimes", "updates", "kanban", "profiles"];
/// Directories of every profile.
pub const PROFILE_DIRS: &[&str] = &[
    "blobs",
    "skills",
    "harness",
    "trajectories",
    "cache",
    "scripts",
    "snapshots",
    "python",
    "memories",
    "cron",
    "logs",
    "themes",
    "plugins",
    "backups",
    "migrations",
];
/// Private files created empty with every profile. `state.db` is created
/// by the store and `secrets.toml` by the file secrets backend.
const PROFILE_FILES: &[&str] = &["grants.toml"];

/// The first contents of a new profile's `config.toml`.
const CONFIG_TEMPLATE: &str = "\
# Prometheus profile configuration. Every key with its default and allowed
# values: `prometheus config docs`. Unset keys use their default.
schema_version = 1
";

/// A validated profile name: lowercase `^[a-z0-9][a-z0-9_-]{0,63}$`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProfileName(String);

impl ProfileName {
    /// Lowercases and checks `name`.
    ///
    /// # Errors
    ///
    /// `profile.invalid_name` (exit 2).
    pub fn parse(name: &str) -> Result<Self, Error> {
        let name = name.to_lowercase();
        let mut chars = name.chars();
        let valid = chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && name.len() <= 64
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        if valid {
            Ok(Self(name))
        } else {
            Err(Error::new(
                &codes::PROFILE_INVALID_NAME,
                format!("invalid profile name '{name}'"),
            )
            .with_hint(
                "use 1-64 characters: a-z, 0-9, '_' or '-', starting with a letter or digit",
            ))
        }
    }

    /// As [`ProfileName::parse`], also refusing the reserved names: the
    /// rule for a name a new profile takes.
    ///
    /// # Errors
    ///
    /// `profile.invalid_name`, `profile.reserved` (exit 2).
    pub fn parse_new(name: &str) -> Result<Self, Error> {
        let name = Self::parse(name)?;
        if RESERVED_NAMES.contains(&name.as_str()) {
            return Err(Error::new(
                &codes::PROFILE_RESERVED,
                format!("profile name '{name}' is reserved"),
            ));
        }
        Ok(name)
    }

    /// The `default` profile.
    #[must_use]
    pub fn default_profile() -> Self {
        Self(DEFAULT_PROFILE.to_owned())
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProfileName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An initialised Prometheus home.
#[derive(Debug, Clone)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    /// Opens the home at `root`, creating the D13 layout and the `default`
    /// profile when missing. A missing `active_profile` means `default`
    /// (`prometheus profile use default` removes the file).
    ///
    /// # Errors
    ///
    /// `home.io` when a directory or file cannot be created.
    pub fn open(root: &Path) -> Result<Self, Error> {
        private_dir(root)?;
        // Windows: everything below inherits the owner-only DACL.
        if let Err(e) = private_fs::restrict_to_owner(root) {
            tracing::warn!("cannot restrict {} to its owner: {e}", root.display());
        }
        for dir in ROOT_DIRS {
            private_dir(&root.join(dir))?;
        }
        tighten(&root.join(ACTIVE_PROFILE_FILE), 0o600);
        let home = Self {
            root: root.to_owned(),
        };
        home.profile(&ProfileName::default_profile())?;
        Ok(home)
    }

    /// The home root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The profile `name`, creating its directory layout when missing.
    ///
    /// # Errors
    ///
    /// `profile.deleted` when the profile was deleted (only `profile
    /// create` revives a name); `home.io` when the layout cannot be created.
    pub fn profile(&self, name: &ProfileName) -> Result<ProfileDir, Error> {
        check_not_deleted(&self.root, name)?;
        let dir = self.profile_path(name);
        let fresh = !dir.exists();
        private_dir(&dir)?;
        for sub in PROFILE_DIRS {
            private_dir(&dir.join(sub))?;
        }
        let config = dir.join("config.toml");
        if !config.exists() {
            write_new(&config, CONFIG_TEMPLATE)?;
        }
        for file in PROFILE_FILES {
            ensure_private_file(&dir.join(file))?;
        }
        if fresh {
            // A new profile starts on the default `file` secrets backend.
            ensure_private_file(&dir.join("secrets.toml"))?;
        }
        tighten(&config, 0o600);
        tighten(&dir.join("secrets.toml"), 0o600);
        Ok(ProfileDir {
            name: name.clone(),
            dir,
            home: self.root.clone(),
        })
    }

    /// The existing profile `name` (its layout is completed if a directory
    /// is missing). `default` always exists.
    ///
    /// # Errors
    ///
    /// `profile.not_found` (usage, exit 2) when the profile was never created;
    /// `home.io`.
    pub fn existing_profile(&self, name: &ProfileName) -> Result<ProfileDir, Error> {
        let exists = self.profile_path(name).is_dir();
        if !exists && name.as_str() != DEFAULT_PROFILE {
            return Err(Error::new(
                &codes::PROFILE_NOT_FOUND,
                format!(
                    "profile '{name}' does not exist; create it with: prometheus profile create {name}"
                ),
            ));
        }
        self.profile(name)
    }

    /// `<home>/profiles/<name>` (whether or not it exists).
    #[must_use]
    pub fn profile_path(&self, name: &ProfileName) -> PathBuf {
        self.root.join("profiles").join(name.as_str())
    }

    /// `<home>/profiles/.deleted/<name>`.
    #[must_use]
    pub fn tombstone(&self, name: &ProfileName) -> PathBuf {
        tombstone(&self.root, name)
    }

    /// The profile named by `active_profile`; `default` when the file is
    /// missing or blank.
    ///
    /// # Errors
    ///
    /// `profile.invalid_name` when the file names an invalid profile,
    /// `home.io` when it cannot be read.
    pub fn active_profile(&self) -> Result<ProfileName, Error> {
        let path = self.root.join(ACTIVE_PROFILE_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => ProfileName::parse(text.trim()),
            Ok(_) => Ok(ProfileName::default_profile()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(ProfileName::default_profile()),
            Err(e) => Err(io_error(&path, &e)),
        }
    }

    /// The profile a process works on: `-p NAME` when given, else
    /// `active_profile`, else `default`.
    ///
    /// # Errors
    ///
    /// As [`ProfileName::parse`] and [`Home::active_profile`].
    pub fn select_profile(&self, flag: Option<&str>) -> Result<ProfileName, Error> {
        match flag {
            Some(name) => ProfileName::parse(name),
            None => self.active_profile(),
        }
    }
}

/// One profile's directory. Only [`Home::profile`] creates it, so holding
/// one means the layout exists.
#[derive(Debug, Clone)]
pub struct ProfileDir {
    name: ProfileName,
    dir: PathBuf,
    home: PathBuf,
}

impl ProfileDir {
    /// The profile name.
    #[must_use]
    pub fn name(&self) -> &ProfileName {
        &self.name
    }

    /// `<home>/profiles/<name>`.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// The home root.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Fails when the profile was deleted after this handle was made: a
    /// long-lived holder (a gateway route, a scheduled job) checks this
    /// before writing so it never resurrects a deleted profile.
    ///
    /// # Errors
    ///
    /// `profile.deleted`.
    pub fn ensure_live(&self) -> Result<(), Error> {
        check_not_deleted(&self.home, &self.name)
    }

    /// The profile's `config.toml` and its writer.
    #[must_use]
    pub fn config_file(&self) -> ConfigFile {
        ConfigFile::new(
            self.dir.join("config.toml"),
            self.dir.join("backups").join("config"),
        )
    }
}

fn tombstone(root: &Path, name: &ProfileName) -> PathBuf {
    root.join("profiles")
        .join(TOMBSTONE_DIR)
        .join(name.as_str())
}

fn check_not_deleted(root: &Path, name: &ProfileName) -> Result<(), Error> {
    if tombstone(root, name).exists() {
        return Err(Error::new(
            &codes::PROFILE_DELETED,
            format!("profile '{name}' was deleted"),
        )
        .with_hint(format!(
            "create it again with: prometheus profile create {name}"
        )));
    }
    Ok(())
}

pub(crate) fn io_error(path: &Path, e: &io::Error) -> Error {
    Error::new(&codes::HOME_IO, format!("{}: {e}", path.display()))
}

pub(crate) fn private_dir(dir: &Path) -> Result<(), Error> {
    private_fs::create_dir(dir).map_err(|e| io_error(dir, &e))?;
    tighten(dir, 0o700);
    Ok(())
}

pub(crate) fn write_new(path: &Path, text: &str) -> Result<(), Error> {
    use std::io::Write as _;
    match private_fs::new_file_options().open(path) {
        Ok(mut file) => file
            .write_all(text.as_bytes())
            .map_err(|e| io_error(path, &e)),
        // Another process created it first; its contents win.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(io_error(path, &e)),
    }
}

fn ensure_private_file(path: &Path) -> Result<(), Error> {
    if path.exists() {
        tighten(path, 0o600);
        return Ok(());
    }
    write_new(path, "")
}

/// Removes group/other permission bits from an existing path, with a WARN.
#[cfg(unix)]
fn tighten(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() || meta.permissions().mode().trailing_zeros() >= 6 {
        return;
    }
    match std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        Ok(()) => tracing::warn!(
            "{} was accessible to other users; tightened to {mode:o}",
            path.display()
        ),
        Err(e) => tracing::warn!("cannot tighten permissions of {}: {e}", path.display()),
    }
}

#[cfg(not(unix))]
fn tighten(_path: &Path, _mode: u32) {}

#[cfg(test)]
mod tests {
    use super::{Home, PROFILE_DIRS, ProfileName, ROOT_DIRS};

    #[test]
    fn fresh_home_has_the_d13_layout() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("home");
        let home = Home::open(&root).unwrap();
        for dir in ROOT_DIRS {
            assert!(root.join(dir).is_dir(), "{dir}");
        }
        assert!(
            !root.join("active_profile").exists(),
            "no active_profile means default"
        );
        let profile = root.join("profiles/default");
        assert!(profile.join("config.toml").is_file());
        assert!(profile.join("grants.toml").is_file());
        assert!(profile.join("secrets.toml").is_file());
        for dir in PROFILE_DIRS {
            assert!(profile.join(dir).is_dir(), "{dir}");
        }
        assert!(
            !root.join("config.toml").exists(),
            "no config at the home root"
        );
        assert_eq!(home.active_profile().unwrap().as_str(), "default");
        crate::parse::config(
            &profile.join("config.toml"),
            &std::fs::read_to_string(profile.join("config.toml")).unwrap(),
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn layout_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("home");
        Home::open(&root).unwrap();
        let mode = |p: &str| {
            std::fs::metadata(root.join(p))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(""), 0o700);
        assert_eq!(mode("profiles/default"), 0o700);
        assert_eq!(mode("profiles/default/blobs"), 0o700);
        assert_eq!(mode("run"), 0o700);
        assert_eq!(mode("profiles/default/config.toml"), 0o600);
        assert_eq!(mode("profiles/default/secrets.toml"), 0o600);
        assert_eq!(mode("profiles/default/grants.toml"), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn loose_modes_are_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("home");
        Home::open(&root).unwrap();
        let config = root.join("profiles/default/config.toml");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        Home::open(&root).unwrap();
        assert_eq!(
            std::fs::metadata(&config).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    /// Spec scenario "Loose permissions": 0644 `secrets.toml` (and
    /// `grants.toml`) become 0600 on start, with a WARN naming the file.
    #[cfg(unix)]
    #[test]
    fn loose_secret_and_grant_files_are_tightened_with_a_warning() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Capture(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Capture {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("home");
        Home::open(&root).unwrap();
        let profile = root.join("profiles/default");
        for file in ["secrets.toml", "grants.toml"] {
            std::fs::set_permissions(profile.join(file), std::fs::Permissions::from_mode(0o644))
                .unwrap();
        }
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || Home::open(&root).unwrap());

        let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        for file in ["secrets.toml", "grants.toml"] {
            let path = profile.join(file);
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert!(
                log.lines()
                    .any(|l| l.contains("WARN") && l.contains(&path.display().to_string())),
                "{log}"
            );
        }
    }

    #[test]
    fn reopening_keeps_existing_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("home");
        let home = Home::open(&root).unwrap();
        std::fs::write(root.join("active_profile"), "work\n").unwrap();
        std::fs::write(
            root.join("profiles/default/config.toml"),
            "[agent]\nmax_iterations = 3\n",
        )
        .unwrap();
        Home::open(&root).unwrap();
        assert_eq!(home.active_profile().unwrap().as_str(), "work");
        assert_eq!(
            std::fs::read_to_string(root.join("profiles/default/config.toml")).unwrap(),
            "[agent]\nmax_iterations = 3\n"
        );
    }

    #[test]
    fn profile_selection_flag_then_active_then_default() {
        let temp = tempfile::tempdir().unwrap();
        let home = Home::open(&temp.path().join("h")).unwrap();
        assert_eq!(home.select_profile(Some("Work")).unwrap().as_str(), "work");
        assert_eq!(home.select_profile(None).unwrap().as_str(), "default");
        std::fs::write(home.root().join("active_profile"), "").unwrap();
        assert_eq!(home.select_profile(None).unwrap().as_str(), "default");
    }

    #[test]
    fn profile_names_are_checked() {
        assert!(ProfileName::parse("a").is_ok());
        assert!(ProfileName::parse("my_bot-2").is_ok());
        for bad in ["", "-x", "../etc", "a b", "a/b", &"a".repeat(65)] {
            let err = ProfileName::parse(bad).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{bad}");
        }
    }

    #[test]
    fn selecting_a_missing_profile_is_a_usage_error() {
        let temp = tempfile::tempdir().unwrap();
        let home = Home::open(&temp.path().join("h")).unwrap();
        let err = home
            .existing_profile(&ProfileName::parse("nope").unwrap())
            .unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(
            home.existing_profile(&ProfileName::default_profile())
                .is_ok()
        );
    }

    #[test]
    fn every_profile_lives_under_profiles() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("h");
        let home = Home::open(&root).unwrap();
        let work = home.profile(&ProfileName::parse("work").unwrap()).unwrap();
        assert_eq!(work.path(), root.join("profiles").join("work"));
        assert!(work.path().join("config.toml").is_file());
    }
}

//! [`ProfileScope`]: the explicit value every profile-data operation takes
//! (spec: configuration-and-profiles, Explicit `ProfileScope`; design D3).
//!
//! A scope is built once by an entry point (the CLI command, a gateway
//! route, a scheduled job, an RPC handler) and carries the profile's
//! directory, an immutable config snapshot and its secrets handle. There is
//! no global "current profile": code that needs profile config, paths or
//! secrets takes a `&ProfileScope`, so a deferred callback reads exactly
//! the profile it was created for. The type has private fields and no
//! `Default`, so a scope cannot be conjured without loading a profile.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;

use crate::layers::{Layered, Overrides};
use crate::layout::{ProfileDir, ProfileName};
use crate::schema::Config;
use crate::secrets::{SecretStore, SecretsHandle};

/// One profile's identity, paths, configuration snapshot and secrets.
/// Cheap to clone; clones share the same snapshot.
#[derive(Debug, Clone)]
pub struct ProfileScope {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    profile: ProfileDir,
    config: Layered,
    secrets: SecretsHandle,
}

/// Whether a scope is the process's launch profile, the only one allowed
/// to see secrets from the launch environment.
#[derive(Debug, Clone, Copy)]
pub enum Launch<'a> {
    /// The profile the process was started for, with its environment.
    Profile(&'a LaunchEnv),
    /// Another profile served by the same process.
    Other,
}

impl ProfileScope {
    /// Loads `profile`: migrates and validates its `config.toml`, applies
    /// `overrides` and freezes the result.
    ///
    /// # Errors
    ///
    /// Configuration errors (exit 78), invalid overrides (exit 2), `home.io`.
    pub fn open(
        profile: ProfileDir,
        overrides: &Overrides,
        launch: Launch<'_>,
    ) -> Result<Self, Error> {
        let config = profile.config_file().load(overrides)?;
        let store = SecretStore::open(&profile, config.config().secrets.backend);
        let launch_env = match launch {
            Launch::Profile(env) => Some(
                env.secrets()
                    .map(|(name, value)| (name.to_owned(), value.clone()))
                    .collect(),
            ),
            Launch::Other => None,
        };
        let secrets = SecretsHandle::new(profile.name().as_str(), store, launch_env);
        Ok(Self {
            inner: Arc::new(Inner {
                profile,
                config,
                secrets,
            }),
        })
    }

    /// The profile name.
    #[must_use]
    pub fn name(&self) -> &ProfileName {
        self.inner.profile.name()
    }

    /// `<home>/profiles/<name>`.
    #[must_use]
    pub fn dir(&self) -> &Path {
        self.inner.profile.path()
    }

    /// The home root (for the machine-wide files).
    #[must_use]
    pub fn home(&self) -> &Path {
        self.inner.profile.home()
    }

    /// A path inside the profile directory.
    #[must_use]
    pub fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.dir().join(relative)
    }

    /// The profile directory handle (for the config writer).
    #[must_use]
    pub fn profile(&self) -> &ProfileDir {
        &self.inner.profile
    }

    /// The immutable configuration snapshot taken when the scope opened.
    #[must_use]
    pub fn config(&self) -> &Config {
        self.inner.config.config()
    }

    /// The snapshot with the layer of every value, for `config explain`.
    #[must_use]
    pub fn layers(&self) -> &Layered {
        &self.inner.config
    }

    /// The profile's secrets.
    #[must_use]
    pub fn secrets(&self) -> &SecretsHandle {
        &self.inner.secrets
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use pmt_core::env::{EnvPolicy, LaunchEnv};

    use super::{Launch, ProfileScope};
    use crate::{Home, Overrides, ProfileName};

    fn launch_env() -> LaunchEnv {
        let vars = [
            ("OPENAI_API_KEY", "sk-from-env-123"),
            ("PMT_SECRET_TOKEN", "tok-123456"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        LaunchEnv::from_vars(
            vars,
            &EnvPolicy {
                provider_secret_names: &["OPENAI_API_KEY"],
                replacements: &[],
            },
        )
    }

    #[test]
    fn deferred_work_keeps_its_own_profile() {
        let temp = tempfile::tempdir().unwrap();
        let home = Home::open(&temp.path().join("h")).unwrap();
        let work = home.profile(&ProfileName::parse("work").unwrap()).unwrap();
        let personal = home.profile(&ProfileName::parse("home").unwrap()).unwrap();
        std::fs::write(
            work.path().join("config.toml"),
            "[agent]\nmax_iterations = 7\n",
        )
        .unwrap();
        std::fs::write(
            personal.path().join("config.toml"),
            "[agent]\nmax_iterations = 9\n",
        )
        .unwrap();
        let env = launch_env();
        let work = ProfileScope::open(work, &Overrides::default(), Launch::Profile(&env)).unwrap();
        let personal = ProfileScope::open(personal, &Overrides::default(), Launch::Other).unwrap();
        // A callback created for `home` and run later, after `work` loaded,
        // still sees `home`'s config, directory and secrets.
        let callback = {
            let scope = personal.clone();
            move || {
                (
                    scope.config().agent.max_iterations,
                    scope.dir().to_owned(),
                    scope
                        .secrets()
                        .launch_env_secret("OPENAI_API_KEY")
                        .is_some(),
                )
            }
        };
        assert_eq!(work.config().agent.max_iterations, 7);
        let (iterations, dir, sees_env_key) = callback();
        assert_eq!(iterations, 9);
        assert!(dir.ends_with("profiles/home"));
        assert!(
            !sees_env_key,
            "the launch environment must not leak to another profile"
        );
        assert_eq!(
            work.secrets()
                .launch_env_secret("OPENAI_API_KEY")
                .unwrap()
                .expose(),
            "sk-from-env-123"
        );
        assert_eq!(
            work.secrets().launch_env_secret("TOKEN").unwrap().expose(),
            "tok-123456"
        );
    }

    #[test]
    fn snapshot_is_frozen_at_open() {
        let temp = tempfile::tempdir().unwrap();
        let home = Home::open(&temp.path().join("h")).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        let scope =
            ProfileScope::open(profile.clone(), &Overrides::default(), Launch::Other).unwrap();
        profile
            .config_file()
            .set("agent.max_iterations", "3")
            .unwrap();
        assert_eq!(scope.config().agent.max_iterations, 500);
        let reopened = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
        assert_eq!(reopened.config().agent.max_iterations, 3);
    }

    #[test]
    fn debug_output_never_shows_secret_values() {
        let temp = tempfile::tempdir().unwrap();
        let home = Home::open(&temp.path().join("h")).unwrap();
        let env = launch_env();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        let scope =
            ProfileScope::open(profile, &Overrides::default(), Launch::Profile(&env)).unwrap();
        let debug = format!("{scope:?}");
        assert!(!debug.contains("sk-from-env-123"), "{debug}");
    }
}

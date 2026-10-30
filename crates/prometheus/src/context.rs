//! What every command that works on a home or profile needs from the
//! process: the `--home`, `-p`, `--set` and `--json` flags and the launch
//! environment read once at startup.

use std::path::Path;

use pmt_config::{Home, Launch, Overrides, ProfileDir, ProfileScope, home};
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;

/// The process-wide inputs of a command.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// `--home`.
    pub home_flag: Option<&'a Path>,
    /// The launch environment, read once at startup.
    pub launch: &'a LaunchEnv,
    /// `-p/--profile`.
    pub profile: Option<&'a str>,
    /// `--set key=value` overrides.
    pub set: &'a [String],
    /// `--json`.
    pub json: bool,
}

impl Context<'_> {
    /// The home, creating its layout when missing.
    ///
    /// # Errors
    ///
    /// `home.unresolved`, `home.io`.
    pub fn home(&self) -> Result<Home, Error> {
        let root = home::resolve(self.home_flag, self.launch)?;
        Home::open(&root)
    }

    /// The selected profile's directory (`-p` > `active_profile` > `default`).
    ///
    /// # Errors
    ///
    /// Home errors, `profile.not_found` (2), `profile.invalid_name`.
    pub fn profile_dir(&self) -> Result<ProfileDir, Error> {
        let home = self.home()?;
        let name = home.select_profile(self.profile)?;
        home.existing_profile(&name)
    }

    /// The selected profile loaded with the `--set` overrides; the launch
    /// environment's secrets are visible to it.
    ///
    /// # Errors
    ///
    /// As [`Context::profile_dir`], configuration errors.
    pub fn scope(&self) -> Result<ProfileScope, Error> {
        let overrides = Overrides::parse(self.set)?;
        ProfileScope::open(
            self.profile_dir()?,
            &overrides,
            Launch::Profile(self.launch),
        )
    }
}

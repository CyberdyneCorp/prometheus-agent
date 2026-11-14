//! What the app reads before its first window opens (spec: desktop-app,
//! Desktop launch options; Launching the desktop from the CLI; Signed
//! in-app updates): the home, the bundled binary, the `[desktop]` options
//! of the active profile, the arguments `prometheus desktop` passes, and
//! whether this build has an updater.

use std::path::PathBuf;

use pmt_desktop::options::{self, Display};
use pmt_desktop::update::{self, HttpFetch, Updater, UpdaterConfig};
use pmt_desktop::update_install::Target;

/// The app's launch.
pub(crate) struct Launch {
    pub(crate) home: PathBuf,
    pub(crate) user_home: PathBuf,
    pub(crate) binary: PathBuf,
    pub(crate) options: options::Read,
    /// `--ignore-existing`: no local daemon is ensured.
    pub(crate) ignore_existing: bool,
    /// `prometheus://` links among the arguments.
    pub(crate) links: Vec<String>,
}

impl Launch {
    /// Reads the launch.
    pub(crate) fn read() -> Result<Self, Box<dyn std::error::Error>> {
        let env = pmt_core::env::LaunchEnv::from_process(&pmt_core::env::EnvPolicy::default())?;
        let home = pmt_config::home::resolve(None, &env)?;
        let user_home = env.user_home().cloned().unwrap_or_else(|| home.clone());
        let args: Vec<String> = std::env::args().skip(1).collect();
        let config = pmt_config::Home::open(&home)
            .and_then(|h| {
                let name = h.select_profile(None)?;
                Ok(h.profile_path(&name).join("config.toml"))
            })
            .ok();
        Ok(Self {
            options: config.as_deref().map(options::read).unwrap_or_default(),
            binary: crate::app::bundled_binary(),
            ignore_existing: args.iter().any(|a| a == "--ignore-existing"),
            links: args
                .into_iter()
                .filter(|a| a.starts_with("prometheus://"))
                .collect(),
            home,
            user_home,
        })
    }

    /// Starts the app again with the webview switches the options ask for
    /// when they are not set yet; the exit code to leave with, or `None`
    /// to go on.
    pub(crate) fn relaunch_with_environment(&self) -> Option<i32> {
        let wanted = self
            .options
            .options
            .environment(std::env::consts::OS, Display::detect());
        let missing: Vec<(&str, String)> = wanted
            .into_iter()
            .filter(|(key, value)| std::env::var(key).ok().as_deref() != Some(value.as_str()))
            .collect();
        if missing.is_empty() {
            return None;
        }
        let exe = std::env::current_exe().ok()?;
        let mut command = std::process::Command::new(exe);
        command.args(std::env::args_os().skip(1)).envs(missing);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            let e = command.exec();
            tracing::warn!("could not restart with the launch options: {e}");
            None
        }
        #[cfg(not(unix))]
        {
            let status = command.status().ok()?;
            Some(status.code().unwrap_or(1))
        }
    }

    /// Linux: keeps the XDG launcher entry as `manage_launcher_entry` says.
    pub(crate) fn keep_launcher_entry(&self) {
        if !cfg!(target_os = "linux") {
            return;
        }
        let exec = std::env::var_os("APPIMAGE")
            .map(PathBuf::from)
            .or_else(|| std::env::current_exe().ok());
        let Some(exec) = exec else {
            return;
        };
        let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
        let at = pmt_desktop::launcher::path(&self.user_home, data.as_deref());
        if let Err(e) =
            pmt_desktop::launcher::ensure(&at, &exec, self.options.options.manage_launcher_entry)
        {
            tracing::warn!("could not write the launcher entry: {e}");
        }
    }

    /// The install the updater replaces and its state directory, when
    /// this install is updated by the desktop updater.
    pub(crate) fn update_target(&self) -> Option<(Target, PathBuf)> {
        let exe = std::env::current_exe().ok()?;
        let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
        let target = Target::detect(&exe, appimage.as_deref())?;
        Some((target, self.home.join("updates").join("desktop-app")))
    }

    /// The updater of this build: none without an embedded release key
    /// (the release workflow sets `PMT_DESKTOP_RELEASE_KEY` at build time).
    pub(crate) fn updater(&self, version: &str) -> Updater {
        let (Some(key), Some((target, dir))) =
            (option_env!("PMT_DESKTOP_RELEASE_KEY"), self.update_target())
        else {
            return Updater::disabled();
        };
        let origin = pmt_config::daemon::DaemonConfig::load(&self.home)
            .ok()
            .and_then(|c| c.update.mirror_url)
            .unwrap_or_else(|| update::DEFAULT_ORIGIN.to_owned());
        let Ok(fetch) = HttpFetch::new(&format!("prometheus-desktop/{version}")) else {
            return Updater::disabled();
        };
        Updater::new(
            UpdaterConfig {
                release_key: Some(key.to_owned()),
                channel: option_env!("PMT_DESKTOP_CHANNEL")
                    .unwrap_or("stable")
                    .to_owned(),
                origin,
                platform: target.platform(),
                current: version.to_owned(),
                dir,
            },
            Box::new(fetch),
        )
    }
}

//! The local browser options of `[browser]` (spec: execution-environments,
//! Local browser options and headed mode).
//!
//! `executable_path` picks the browser binary (empty: the managed
//! `chromium` runtime, then a host browser), `user_data_dir` a persistent
//! profile directory (empty: a throwaway one), `chromium_args` extra
//! switches, `launch_timeout_secs` the start bound and `headed` a visible
//! window. Extra switches that turn off Chromium's own sandbox or its web
//! security are refused unless `security.sandbox.allow_degraded` names
//! `chromium_sandbox` or `web_security` (or `all`): they weaken isolation
//! as much as a missing OS sandbox feature does.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pmt_config::schema::{BrowserConfig, DegradedFeature};

/// What a local launch takes from the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOptions {
    /// Bound on the start (`browser.launch_timeout_secs`).
    pub timeout: Duration,
    /// Open a visible window.
    pub headed: bool,
    /// A persistent profile directory, kept when the browser ends.
    pub user_data_dir: Option<PathBuf>,
    /// Extra switches, already checked by [`check_args`].
    pub extra_args: Vec<String>,
    /// The display variables the browser needs for a window.
    pub display_env: Vec<(String, OsString)>,
}

impl Default for LaunchOptions {
    /// Headless on a throwaway profile, bounded by the default 30 s.
    fn default() -> Self {
        Self {
            timeout: super::launch::LAUNCH_TIMEOUT,
            headed: false,
            user_data_dir: None,
            extra_args: Vec::new(),
            display_env: Vec::new(),
        }
    }
}

/// The display of this host as a headed browser needs it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Display {
    /// A window can be opened (always on macOS and Windows; elsewhere with
    /// `DISPLAY` or `WAYLAND_DISPLAY`).
    pub available: bool,
    /// The variables a browser needs to reach it (`DISPLAY`,
    /// `WAYLAND_DISPLAY`, `XAUTHORITY`, `XDG_RUNTIME_DIR`).
    pub env: Vec<(String, OsString)>,
}

/// Variables that carry a display to a child.
pub const DISPLAY_VARS: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "XDG_RUNTIME_DIR",
];

impl Display {
    /// The display of the process launched with `launch`.
    #[must_use]
    pub fn of(launch: &pmt_core::env::LaunchEnv) -> Self {
        Self {
            available: launch.has_graphical_display(),
            env: DISPLAY_VARS
                .iter()
                .filter_map(|name| {
                    launch
                        .inherited(name)
                        .map(|value| ((*name).to_owned(), value.to_owned()))
                })
                .collect(),
        }
    }

    /// No display (a headless server).
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }
}

/// The notice of a headed browser started headless.
pub const NO_DISPLAY_NOTICE: &str =
    "browser.headed is set but no display is available: the browser runs headless";

/// The start bound of `config`.
#[must_use]
pub fn launch_timeout(config: &BrowserConfig) -> Duration {
    Duration::from_secs(config.launch_timeout_secs.clamp(1, 600))
}

/// `browser.user_data_dir` resolved: relative paths under `profile_dir`;
/// `None` when empty.
#[must_use]
pub fn user_data_dir(config: &BrowserConfig, profile_dir: &Path) -> Option<PathBuf> {
    let dir = config.user_data_dir.trim();
    if dir.is_empty() {
        return None;
    }
    let path = Path::new(dir);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        profile_dir.join(path)
    })
}

/// `browser.executable_path`, when set.
#[must_use]
pub fn executable(config: &BrowserConfig) -> Option<PathBuf> {
    let path = config.executable_path.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Switches that turn off Chromium's own sandbox.
const SANDBOX_SWITCHES: &[&str] = &[
    "--no-sandbox",
    "--disable-setuid-sandbox",
    "--disable-seccomp-filter-sandbox",
    "--disable-gpu-sandbox",
    "--no-zygote-sandbox",
    "--disable-namespace-sandbox",
];

/// Switches that turn off web security (same-origin policy, site
/// isolation, mixed-content blocking).
const WEB_SECURITY_SWITCHES: &[&str] = &[
    "--disable-web-security",
    "--disable-site-isolation-trials",
    "--allow-running-insecure-content",
];

/// Switches the launch owns: the profile directory and the DevTools
/// transport are chosen by the launch, never by configuration.
const OWNED_SWITCHES: &[&str] = &[
    "--user-data-dir",
    "--remote-debugging-port",
    "--remote-debugging-address",
    "--remote-debugging-pipe",
    "--proxy-server",
    "--proxy-bypass-list",
    "--proxy-pac-url",
    "--headless",
];

/// The feature an extra switch weakens, if any.
fn weakens(arg: &str) -> Option<DegradedFeature> {
    let flag = arg.split_once('=').map_or(arg, |(flag, _)| flag);
    if SANDBOX_SWITCHES.contains(&flag) {
        return Some(DegradedFeature::ChromiumSandbox);
    }
    let isolation_off = flag == "--disable-features"
        && arg.split_once('=').is_some_and(|(_, list)| {
            list.split(',')
                .any(|f| f.starts_with("IsolateOrigins") || f == "site-per-process")
        });
    (WEB_SECURITY_SWITCHES.contains(&flag) || isolation_off).then_some(DegradedFeature::WebSecurity)
}

/// Checks `browser.chromium_args` against `allowed`
/// (`security.sandbox.allow_degraded`).
///
/// # Errors
///
/// The first switch that weakens isolation without its entry, or one the
/// launch owns (profile, DevTools transport, proxy, headless).
pub fn check_args(args: &[String], allowed: &[DegradedFeature]) -> Result<(), String> {
    for arg in args {
        let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
        if OWNED_SWITCHES.contains(&flag) {
            return Err(format!(
                "browser.chromium_args: '{arg}' is set by Prometheus itself (use browser.user_data_dir, browser.headed or browser.cdp_url)"
            ));
        }
        if let Some(feature) = weakens(arg)
            && !allowed.contains(&feature)
            && !allowed.contains(&DegradedFeature::All)
        {
            return Err(format!(
                "browser.chromium_args: '{arg}' would weaken browser isolation; remove it, or set security.sandbox.allow_degraded = [\"{}\"]",
                feature.as_str()
            ));
        }
    }
    Ok(())
}

/// The launch options of `config` on `display`, and the notice to give
/// when a headed browser has to run headless.
///
/// # Errors
///
/// A refused `chromium_args` switch.
pub fn resolve(
    config: &BrowserConfig,
    profile_dir: &Path,
    allowed: &[DegradedFeature],
    display: &Display,
) -> Result<(LaunchOptions, Option<&'static str>), String> {
    check_args(&config.chromium_args, allowed)?;
    let headed = config.headed && display.available;
    let notice = (config.headed && !display.available).then_some(NO_DISPLAY_NOTICE);
    Ok((
        LaunchOptions {
            timeout: launch_timeout(config),
            headed,
            user_data_dir: user_data_dir(config, profile_dir),
            extra_args: config.chromium_args.clone(),
            display_env: if headed {
                display.env.clone()
            } else {
                Vec::new()
            },
        },
        notice,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn switches_that_weaken_isolation_need_their_downgrade() {
        let plain = args(&["--lang=fr", "--window-size=800,600"]);
        assert!(check_args(&plain, &[]).is_ok());
        let no_sandbox = args(&["--no-sandbox"]);
        let refused = check_args(&no_sandbox, &[]).unwrap_err();
        assert!(refused.contains("--no-sandbox"), "{refused}");
        assert!(refused.contains("chromium_sandbox"), "{refused}");
        assert!(check_args(&no_sandbox, &[DegradedFeature::ChromiumSandbox]).is_ok());
        assert!(check_args(&no_sandbox, &[DegradedFeature::WebSecurity]).is_err());
        let web = args(&["--disable-web-security"]);
        assert!(check_args(&web, &[]).unwrap_err().contains("web_security"));
        assert!(check_args(&web, &[DegradedFeature::WebSecurity]).is_ok());
        assert!(check_args(&web, &[DegradedFeature::All]).is_ok());
        let isolation = args(&["--disable-features=Translate,IsolateOrigins"]);
        assert!(check_args(&isolation, &[]).is_err());
    }

    #[test]
    fn switches_the_launch_owns_are_refused() {
        for owned in [
            "--user-data-dir=/tmp/x",
            "--remote-debugging-port=9222",
            "--headless",
        ] {
            let refused = check_args(&args(&[owned]), &[DegradedFeature::All]).unwrap_err();
            assert!(refused.contains(owned), "{refused}");
        }
    }

    /// Scenario: Headless server.
    #[test]
    fn headed_without_a_display_runs_headless_with_a_notice() {
        let config = BrowserConfig {
            headed: true,
            ..BrowserConfig::default()
        };
        let (options, notice) = resolve(&config, Path::new("/p"), &[], &Display::none()).unwrap();
        assert!(!options.headed);
        assert_eq!(notice, Some(NO_DISPLAY_NOTICE));
        let screen = Display {
            available: true,
            env: vec![("DISPLAY".to_owned(), ":0".into())],
        };
        let (options, notice) = resolve(&config, Path::new("/p"), &[], &screen).unwrap();
        assert!(options.headed);
        assert_eq!(options.display_env, screen.env);
        assert_eq!(notice, None);
    }

    #[test]
    fn a_relative_user_data_dir_resolves_under_the_profile() {
        let mut config = BrowserConfig::default();
        assert_eq!(user_data_dir(&config, Path::new("/p")), None);
        config.user_data_dir = "browser/main".to_owned();
        assert_eq!(
            user_data_dir(&config, Path::new("/p")),
            Some(PathBuf::from("/p/browser/main"))
        );
    }
}

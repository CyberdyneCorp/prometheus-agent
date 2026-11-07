//! Which Chromium-family browser is the user's default, where it keeps its
//! profiles and its executable (spec: execution-environments, Real-profile
//! browsing).
//!
//! The default browser comes from the platform's own setting: macOS
//! `LaunchServices`' `https` handler, `xdg-settings` on Linux and the `https`
//! `UserChoice` on Windows. A default browser outside the Chromium family
//! (Safari, Firefox) cannot be browsed with a profile copy.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A Chromium-family browser installed on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Browser {
    /// Short name: the snapshot directory (`chrome`, `brave`...).
    pub name: String,
    /// Its executable.
    pub executable: PathBuf,
    /// Its user data directory (the parent of `Default`, `Profile 1`...).
    pub user_data_dir: PathBuf,
}

/// Finds the user's default browser.
pub trait Detect: Send + Sync {
    /// The default browser.
    ///
    /// # Errors
    ///
    /// Why it cannot be used: not Chromium-family, not found, unreadable
    /// setting.
    fn default_browser(&self) -> Result<Browser, String>;
}

/// One known browser: its platform identifiers and locations.
struct Known {
    name: &'static str,
    /// macOS bundle id, `xdg-settings` desktop file, Windows `ProgId`.
    mac_bundle: &'static str,
    desktop: &'static [&'static str],
    prog_id: &'static str,
    /// User data directory under the platform's base (Application
    /// Support, `~/.config`, `%LOCALAPPDATA%`).
    mac_dir: &'static str,
    linux_dir: &'static str,
    windows_dir: &'static str,
    mac_app: &'static str,
    linux_exe: &'static [&'static str],
    windows_exe: &'static str,
}

const KNOWN: &[Known] = &[
    Known {
        name: "chrome",
        mac_bundle: "com.google.chrome",
        desktop: &["google-chrome.desktop", "com.google.Chrome.desktop"],
        prog_id: "ChromeHTML",
        mac_dir: "Google/Chrome",
        linux_dir: "google-chrome",
        windows_dir: r"Google\Chrome\User Data",
        mac_app: "Google Chrome.app/Contents/MacOS/Google Chrome",
        linux_exe: &["google-chrome", "google-chrome-stable"],
        windows_exe: r"Google\Chrome\Application\chrome.exe",
    },
    Known {
        name: "chromium",
        mac_bundle: "org.chromium.chromium",
        desktop: &["chromium.desktop", "chromium-browser.desktop"],
        prog_id: "ChromiumHTM",
        mac_dir: "Chromium",
        linux_dir: "chromium",
        windows_dir: r"Chromium\User Data",
        mac_app: "Chromium.app/Contents/MacOS/Chromium",
        linux_exe: &["chromium", "chromium-browser"],
        windows_exe: r"Chromium\Application\chrome.exe",
    },
    Known {
        name: "brave",
        mac_bundle: "com.brave.browser",
        desktop: &["brave-browser.desktop", "com.brave.Browser.desktop"],
        prog_id: "BraveHTML",
        mac_dir: "BraveSoftware/Brave-Browser",
        linux_dir: "BraveSoftware/Brave-Browser",
        windows_dir: r"BraveSoftware\Brave-Browser\User Data",
        mac_app: "Brave Browser.app/Contents/MacOS/Brave Browser",
        linux_exe: &["brave-browser", "brave"],
        windows_exe: r"BraveSoftware\Brave-Browser\Application\brave.exe",
    },
    Known {
        name: "edge",
        mac_bundle: "com.microsoft.edgemac",
        desktop: &["microsoft-edge.desktop"],
        prog_id: "MSEdgeHTM",
        mac_dir: "Microsoft Edge",
        linux_dir: "microsoft-edge",
        windows_dir: r"Microsoft\Edge\User Data",
        mac_app: "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        linux_exe: &["microsoft-edge", "microsoft-edge-stable"],
        windows_exe: r"Microsoft\Edge\Application\msedge.exe",
    },
];

/// The default browser of this host, read from the platform setting.
pub struct HostDetect {
    /// The user's home.
    pub user_home: PathBuf,
    /// `PATH` (Linux executables).
    pub path_var: Option<std::ffi::OsString>,
}

impl Detect for HostDetect {
    fn default_browser(&self) -> Result<Browser, String> {
        let id = platform_default()?;
        let known = KNOWN.iter().find(|k| matches_id(k, &id)).ok_or_else(|| {
            format!("the default browser ({id}) is not a Chromium-family browser")
        })?;
        let browser = Browser {
            name: known.name.to_owned(),
            executable: self.executable(known)?,
            user_data_dir: self.user_data_dir(known),
        };
        Ok(browser)
    }
}

fn matches_id(known: &Known, id: &str) -> bool {
    let id = id.trim();
    known.mac_bundle.eq_ignore_ascii_case(id)
        || known.desktop.iter().any(|d| d.eq_ignore_ascii_case(id))
        || id.starts_with(known.prog_id)
}

impl HostDetect {
    fn user_data_dir(&self, known: &Known) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.user_home
                .join("Library/Application Support")
                .join(known.mac_dir)
        } else if cfg!(windows) {
            let local = std::env::var_os("LOCALAPPDATA")
                .map_or_else(|| self.user_home.join(r"AppData\Local"), PathBuf::from);
            local.join(known.windows_dir)
        } else {
            self.user_home.join(".config").join(known.linux_dir)
        }
    }

    fn executable(&self, known: &Known) -> Result<PathBuf, String> {
        let mut candidates = Vec::new();
        if cfg!(target_os = "macos") {
            candidates.push(Path::new("/Applications").join(known.mac_app));
            candidates.push(self.user_home.join("Applications").join(known.mac_app));
        } else if cfg!(windows) {
            for root in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
                if let Some(base) = std::env::var_os(root) {
                    candidates.push(PathBuf::from(base).join(known.windows_exe));
                }
            }
        } else if let Some(path) = &self.path_var {
            for name in known.linux_exe {
                candidates.extend(crate::terminal::shell::find_all(name, path, None));
            }
        }
        candidates.into_iter().find(|p| p.is_file()).ok_or_else(|| {
            format!(
                "the default browser ({}) is not installed where expected",
                known.name
            )
        })
    }
}

/// The platform's identifier of the default `https` handler.
fn platform_default() -> Result<String, String> {
    if cfg!(target_os = "macos") {
        let text = output(
            "/usr/bin/defaults",
            &[
                "read",
                "com.apple.LaunchServices/com.apple.launchservices.secure",
                "LSHandlers",
            ],
        )?;
        mac_https_handler(&text).ok_or_else(|| "no default browser is set".to_owned())
    } else if cfg!(windows) {
        let text = output(
            "reg",
            &[
                "query",
                r"HKCU\Software\Microsoft\Windows\Shell\Associations\UrlAssociations\https\UserChoice",
                "/v",
                "ProgId",
            ],
        )?;
        text.split_whitespace()
            .last()
            .map(str::to_owned)
            .ok_or_else(|| "no default browser is set".to_owned())
    } else {
        output("xdg-settings", &["get", "default-web-browser"]).map(|s| s.trim().to_owned())
    }
}

fn output(program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|err| format!("cannot read the default browser ({program}: {err})"))?;
    if !out.status.success() {
        return Err(format!(
            "cannot read the default browser ({program} failed)"
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The `LSHandlerRoleAll` of the `https` entry in `defaults read` output.
fn mac_https_handler(text: &str) -> Option<String> {
    text.split('}').find_map(|entry| {
        let https = entry.contains("LSHandlerURLScheme = https;");
        let role = entry
            .lines()
            .find_map(|line| line.trim().strip_prefix("LSHandlerRoleAll = "))?;
        https.then(|| role.trim_end_matches(';').trim_matches('"').to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_https_handler_is_read_from_launch_services() {
        let text = r#"(
    {
        LSHandlerContentType = "public.html";
        LSHandlerRoleAll = "com.apple.safari";
    },
    {
        LSHandlerPreferredVersions = { LSHandlerRoleAll = "-"; };
        LSHandlerRoleAll = "com.brave.browser";
        LSHandlerURLScheme = https;
    }
)"#;
        assert_eq!(
            mac_https_handler(text).as_deref(),
            Some("com.brave.browser")
        );
    }

    #[test]
    fn platform_ids_name_chromium_family_browsers() {
        let find = |id: &str| KNOWN.iter().find(|k| matches_id(k, id)).map(|k| k.name);
        assert_eq!(find("com.google.chrome"), Some("chrome"));
        assert_eq!(find("brave-browser.desktop"), Some("brave"));
        assert_eq!(find("MSEdgeHTM"), Some("edge"));
        assert_eq!(find("firefox.desktop"), None);
        assert_eq!(find("com.apple.safari"), None);
    }
}

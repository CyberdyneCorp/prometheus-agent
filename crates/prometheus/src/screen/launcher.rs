//! The desktop's browser launcher (spec: execution-environments, Bot
//! screen: "while the screen runs, the bot's browser and the desktop's
//! browser launcher SHALL share one persistent per-profile user-data
//! directory"): a Chromium-family browser on the running screen with the
//! profile's screen browser profile. When the agent's headed browser is
//! already open on that profile, Chromium hands the page to it, so the
//! person and the agent see the same windows, cookies and sign-ins.
//!
//! The browser is the person's: it runs with a minimal environment (none
//! of the daemon's secrets) and outside the agent's OS sandbox, like a
//! browser they started themselves on that display.

use std::ffi::OsStr;
use std::process::{Command, Stdio};

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_rpc::contracts as c;

/// Variables the browser gets from the daemon besides `DISPLAY` and
/// `XAUTHORITY`.
const ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "TZ",
    "XDG_RUNTIME_DIR",
];

/// Opens `url` (a blank page when `None`) in a browser on the running
/// screen of `scope`.
///
/// # Errors
///
/// `screen.unavailable` (no screen, no browser, or it did not start);
/// `rpc.invalid_params` for a page that is not `http`/`https`.
pub async fn open(
    scope: &ProfileScope,
    url: Option<&str>,
    path_var: Option<&OsStr>,
) -> Result<c::ScreenBrowserOpened, Error> {
    let page = page(url)?;
    let Some((record, _)) = super::running(scope) else {
        return Err(
            Error::new(&codes::SCREEN_UNAVAILABLE, "the bot screen is not running")
                .with_hint("prometheus computer-use screen start"),
        );
    };
    let config = scope.config();
    let executable = match pmt_tools::browser::options::executable(&config.browser) {
        Some(path) => path,
        None => {
            crate::runtimes_cmd::browser::for_home(scope.home(), &config.network, None, path_var)?
                .chromium
                .executable("chromium")
                .await
                .map_err(|why| unavailable(&why))?
        }
    };
    let profile = super::browser_profile(scope);
    pmt_core::private_fs::create_dir(&profile)
        .map_err(|e| Error::io(&codes::HOME_IO, &profile, &e))?;
    let mut command = Command::new(&executable);
    command
        .env_clear()
        .envs(
            ENV.iter()
                .filter_map(|n| std::env::var_os(n).map(|v| (*n, v))),
        )
        .env("DISPLAY", &record.display)
        .envs(record.xauthority.iter().map(|path| ("XAUTHORITY", path)))
        .arg(format!("--user-data-dir={}", profile.display()))
        .args(["--no-first-run", "--no-default-browser-check", &page])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    pmt_tools::browser::cleanup::new_group(&mut command);
    let mut child = command
        .spawn()
        .map_err(|e| unavailable(&format!("{}: {e}", executable.display())))?;
    // The browser outlives this call; reap it whenever it exits.
    std::thread::spawn(move || child.wait());
    super::touch(scope);
    Ok(c::ScreenBrowserOpened {
        user_data_dir: profile.display().to_string(),
    })
}

/// The page to open: an `http`/`https` URL, else a blank page.
fn page(url: Option<&str>) -> Result<String, Error> {
    let Some(url) = url.map(str::trim).filter(|u| !u.is_empty()) else {
        return Ok("about:blank".to_owned());
    };
    match url::Url::parse(url) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => Ok(parsed.to_string()),
        _ => Err(Error::new(
            &codes::RPC_INVALID_PARAMS,
            format!("'{url}' is not an http or https page"),
        )),
    }
}

fn unavailable(why: &str) -> Error {
    Error::new(
        &codes::SCREEN_UNAVAILABLE,
        format!("cannot open a browser on the bot screen: {why}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_pages_open() {
        assert_eq!(page(None).unwrap(), "about:blank");
        assert_eq!(
            page(Some("https://example.org")).unwrap(),
            "https://example.org/"
        );
        assert!(page(Some("file:///etc/passwd")).is_err());
        assert!(page(Some("--disable-web-security")).is_err());
    }
}

//! Windows never open themselves (spec: desktop-app, Windows never open
//! themselves). Page content cannot open windows at all, and a window's
//! top-level document may only navigate within the app's own origin; links
//! leave the app only through the external URL command after a trusted
//! click.

use url::Url;

/// The origins the webview serves the app from (the ones the daemon's RPC
/// listener admits).
pub const APP_ORIGINS: &[&str] = pmt_rpc::origins::DESKTOP_ORIGINS;

/// Whether a window's top-level document may navigate to `url`.
#[must_use]
pub fn allow_navigation(url: &Url) -> bool {
    let origin = url.origin().ascii_serialization();
    // `tauri://localhost` has an opaque origin in the URL standard.
    let custom = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
    APP_ORIGINS.contains(&origin.as_str()) || APP_ORIGINS.contains(&custom.as_str())
}

/// Whether `url` is a plugin panel's document (`plugin-<id>://localhost`,
/// or `http://plugin-<id>.localhost` on Windows): what the panel frames
/// of the window load. Their frames are sandboxed without top
/// navigation, so this never lets a panel replace the app.
#[must_use]
pub fn is_panel(url: &Url) -> bool {
    let host = url.host_str().unwrap_or_default();
    (url.scheme().starts_with("plugin-") && host == "localhost")
        || (url.scheme() == "http"
            && host
                .strip_suffix(".localhost")
                .is_some_and(|h| h.starts_with("plugin-")))
}

/// Whether page content may open a new window (`window.open`, a
/// `target=_blank` link): never.
#[must_use]
pub const fn allow_new_window(_url: &Url) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::{allow_navigation, allow_new_window};

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn only_the_app_origin_is_navigable() {
        assert!(allow_navigation(&url("tauri://localhost/index.html")));
        assert!(allow_navigation(&url("http://tauri.localhost/#/chat")));
        assert!(!allow_navigation(&url("https://attacker.example/")));
        assert!(!allow_navigation(&url("http://127.0.0.1:9120/")));
        assert!(!allow_navigation(&url("tauri://evil/")));
        assert!(!allow_navigation(&url("file:///etc/passwd")));
    }

    #[test]
    fn an_artifact_script_opens_no_window() {
        assert!(!allow_new_window(&url("https://attacker.example")));
        assert!(!allow_new_window(&url("tauri://localhost/index.html")));
    }
}

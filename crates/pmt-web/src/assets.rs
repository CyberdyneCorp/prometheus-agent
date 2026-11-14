//! The SPA, compiled into the binary (spec: web-dashboard, SPA is embedded
//! in the binary): `web/dist` is built from `web/src` by `web/build.mjs`
//! and checked in, so a missing or partial frontend build cannot happen at
//! runtime. Each asset is served under a name carrying a hash of its bytes
//! (`/assets/app-<hash>.js`), cached for a year; the shell (`index.html`)
//! is never cached and names the current hashes under the base path.

use std::sync::LazyLock;

use sha2::{Digest as _, Sha256};

/// One embedded asset.
#[derive(Debug)]
pub(crate) struct Asset {
    /// Its hashed file name under `/assets/`.
    pub name: String,
    pub bytes: &'static [u8],
    pub content_type: &'static str,
}

/// The shell template: `%BASE%`, `%APP_JS%` and `%APP_CSS%` are replaced.
const SHELL: &str = include_str!("../../../web/dist/index.html");

static ASSETS: LazyLock<Vec<Asset>> = LazyLock::new(|| {
    [
        (
            "app",
            "js",
            &include_bytes!("../../../web/dist/app.js")[..],
            "text/javascript; charset=utf-8",
        ),
        (
            "app",
            "css",
            &include_bytes!("../../../web/dist/app.css")[..],
            "text/css; charset=utf-8",
        ),
    ]
    .into_iter()
    .map(|(stem, ext, bytes, content_type)| {
        let digest = Sha256::digest(bytes);
        let hash = crate::auth::secret::hex(&digest[..6]);
        Asset {
            name: format!("{stem}-{hash}.{ext}"),
            bytes,
            content_type,
        }
    })
    .collect()
});

/// The asset served as `/assets/<name>`.
pub(crate) fn asset(name: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|a| a.name == name)
}

/// The SPA shell for a dashboard under `base` (`""` or `/prometheus`).
pub(crate) fn shell(base: &str) -> String {
    let name = |ext: &str| {
        ASSETS
            .iter()
            .find(|a| a.name.ends_with(ext))
            .map_or("", |a| a.name.as_str())
    };
    SHELL
        .replace("%APP_JS%", name(".js"))
        .replace("%APP_CSS%", name(".css"))
        .replace("%BASE%", base)
}

/// The desktop app's page (`apps/desktop/dist`, built by `web/build.mjs`
/// and checked in), served as the webapp under `/app` (spec:
/// web-dashboard, Desktop workspace in the browser): the workspace is
/// rendered by the desktop's own components.
const DESKTOP_SHELL: &str = include_str!("../../../apps/desktop/dist/index.html");

/// The desktop page's files, by their path below `/app/`.
const DESKTOP_FILES: &[(&str, &[u8], &str)] = &[
    (
        "app.js",
        include_bytes!("../../../apps/desktop/dist/app.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "app.css",
        include_bytes!("../../../apps/desktop/dist/app.css"),
        "text/css; charset=utf-8",
    ),
    (
        "annotate.js",
        include_bytes!("../../../apps/desktop/dist/annotate.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "fonts/jetbrains-mono-latin-400-normal.woff2",
        include_bytes!("../../../apps/desktop/dist/fonts/jetbrains-mono-latin-400-normal.woff2"),
        "font/woff2",
    ),
    (
        "fonts/jetbrains-mono-latin-ext-400-normal.woff2",
        include_bytes!(
            "../../../apps/desktop/dist/fonts/jetbrains-mono-latin-ext-400-normal.woff2"
        ),
        "font/woff2",
    ),
    (
        "fonts/JetBrainsMono-OFL.txt",
        include_bytes!("../../../apps/desktop/dist/fonts/JetBrainsMono-OFL.txt"),
        "text/plain; charset=utf-8",
    ),
];

/// A file of the desktop page (`/app/<name>`): its bytes and type.
pub(crate) fn desktop_file(name: &str) -> Option<(&'static [u8], &'static str)> {
    DESKTOP_FILES
        .iter()
        .find(|(path, _, _)| *path == name)
        .map(|(_, bytes, content_type)| (*bytes, *content_type))
}

/// The desktop page for a dashboard under `base`: its relative URLs
/// resolve below `<base>/app/`, and the page knows it runs as the webapp
/// and where the dashboard's API is.
pub(crate) fn desktop_shell(base: &str) -> String {
    DESKTOP_SHELL.replacen(
        "<head>",
        &format!(
            "<head>\n<base href=\"{base}/app/\">\n<meta name=\"prometheus-base\" content=\"{base}\">\n<meta name=\"prometheus-webapp\" content=\"1\">"
        ),
        1,
    )
}

#[cfg(test)]
mod tests {
    use super::{asset, desktop_file, desktop_shell, shell};

    /// The webapp page names only files it serves, below the base path.
    #[test]
    fn the_desktop_page_resolves_its_files_below_app() {
        let page = desktop_shell("/prometheus");
        assert!(page.contains("<base href=\"/prometheus/app/\">"), "{page}");
        assert!(page.contains("name=\"prometheus-webapp\""), "{page}");
        for name in ["app.js", "app.css"] {
            assert!(page.contains(&format!("\"{name}\"")), "{page}");
            assert!(desktop_file(name).is_some(), "{name}");
        }
        assert!(desktop_file("../web/dist/app.js").is_none());
    }

    /// Scenario: Behind a proxy at /prometheus (asset URLs).
    #[test]
    fn every_asset_url_of_the_shell_is_under_the_base_path() {
        let page = shell("/prometheus");
        let urls: Vec<&str> = page
            .split(['"', '\''])
            .filter(|part| part.contains("/assets/"))
            .collect();
        assert_eq!(urls.len(), 2, "{page}");
        for url in urls {
            assert!(url.starts_with("/prometheus/assets/"), "{url}");
            let name = url.trim_start_matches("/prometheus/assets/");
            assert!(asset(name).is_some(), "{name}");
        }
        assert!(!page.contains('%'), "{page}");
    }
}

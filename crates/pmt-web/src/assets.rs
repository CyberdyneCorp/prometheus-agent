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

#[cfg(test)]
mod tests {
    use super::{asset, shell};

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

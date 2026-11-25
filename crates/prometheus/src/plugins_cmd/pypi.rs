//! `prometheus plugins install pypi:<dist>==<version>` (spec:
//! plugin-system, Python package and Nix plugin sources).
//!
//! The distribution's pure-Python wheel is found on the package index's
//! PEP 503 page (`daemon.toml` `runtimes.mirrors.pypi`, else uv's `UV_DEFAULT_INDEX`,
//! else `PyPI`), checked against the page's `sha256`, and its
//! `hermes_agent.plugins` (or `prometheus.plugins`) entry point becomes a
//! kind `python` plugin: the wheel's files under `site/` (what the audit
//! reads is what runs), a `plugin.yaml` whose only requirement is the
//! distribution at that exact version (so its dependencies join the
//! plugin's own pinned bridge environment), and an `__init__.py` that
//! loads the entry point. The install record says `pypi`. Index packages
//! carry no publisher signature, so `plugins.signature_policy` applies as
//! for a local package.

use std::io::Read as _;
use std::path::Path;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_plugins::install::{self, Trust};
use pmt_plugins::record::{Flags, InstallRecord, Origin};
use sha2::{Digest as _, Sha256};

use super::InstallArgs;
use super::install::Fetched;
use crate::plugins_host;

/// The identifier prefix.
pub(super) const PREFIX: &str = "pypi:";
/// The entry-point groups a plugin distribution may declare, in order.
const GROUPS: &[&str] = &["prometheus.plugins", "hermes_agent.plugins"];
/// Where the index is when nothing names another.
const PYPI: &str = "https://pypi.org/simple/";

fn unpinned() -> Error {
    Error::new(
        &codes::CLI_USAGE,
        "pypi plugins need an exact version: pypi:<dist>==<version>",
    )
    .with_hint("an unpinned package can change under you; name the version you reviewed")
}

/// `pypi:<dist>==<version>` split, or the usage error.
fn parse(identifier: &str) -> Result<(String, String), Error> {
    let rest = identifier.strip_prefix(PREFIX).unwrap_or(identifier);
    let (dist, version) = rest.split_once("==").ok_or_else(unpinned)?;
    let name_ok = !dist.is_empty()
        && dist
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    let version_ok = !version.is_empty()
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._+!".contains(c));
    if !name_ok || !version_ok {
        return Err(unpinned());
    }
    Ok((dist.to_owned(), version.to_owned()))
}

/// PEP 503 project name.
fn normalized(dist: &str) -> String {
    dist.to_ascii_lowercase().replace(['_', '.'], "-")
}

fn index_url(scope: &ProfileScope) -> String {
    let configured = pmt_config::DaemonConfig::load(scope.home())
        .ok()
        .and_then(|d| d.runtimes.mirrors.pypi);
    let url = configured
        .or_else(|| std::env::var("UV_DEFAULT_INDEX").ok())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| PYPI.to_owned());
    if url.ends_with('/') {
        url
    } else {
        format!("{url}/")
    }
}

fn failed(message: String) -> Error {
    Error::new(&codes::PLUGINS_CATALOG, message)
}

/// Stages `pypi:<dist>==<version>`.
pub(super) fn fetch(
    scope: &ProfileScope,
    identifier: &str,
    args: &InstallArgs,
) -> Result<Fetched, Error> {
    fetch_as(scope, identifier, args, None)
}

/// [`fetch`], the plugin named `id` instead of its entry-point name (the
/// Nix module's `nix-managed-<id>`).
pub(super) fn fetch_as(
    scope: &ProfileScope,
    identifier: &str,
    args: &InstallArgs,
    id: Option<&str>,
) -> Result<Fetched, Error> {
    let (dist, version) = parse(identifier)?;
    let page_url = format!("{}{}/", index_url(scope), normalized(&dist));
    let page = plugins_host::catalog::download(scope, &page_url)?
        .ok_or_else(|| failed(format!("the package index has no project '{dist}'")))?;
    let (wheel_url, sha256) =
        pick_wheel(&String::from_utf8_lossy(&page), &page_url, &dist, &version)?;
    let wheel = plugins_host::catalog::download(scope, &wheel_url)?
        .ok_or_else(|| failed(format!("{wheel_url} was not found")))?;
    let digest = pmt_pybridge::hex(&Sha256::digest(&wheel));
    if !digest.eq_ignore_ascii_case(&sha256) {
        return Err(Error::new(
            &codes::PLUGINS_SIGNATURE,
            format!("{wheel_url} does not match the index's sha256 {sha256}"),
        ));
    }
    let dir = install::staging_dir(scope.dir())?;
    let ready = materialize(&wheel, &dir, (&dist, &version), id);
    let staged = install::stage_dir(dir, &plugins_host::running_version(), ready)?;
    let trust = Trust::Unsigned("index packages carry no publisher signature".to_owned());
    let policy = scope.config().plugins.signature_policy;
    let unsigned = install::admit(&trust, false, policy, args.allow_unsigned)?;
    let record = InstallRecord {
        source: Origin::Pypi,
        location: format!("{PREFIX}{dist}=={version}"),
        r#ref: None,
        package_digest: Some(format!("sha256:{digest}")),
        digest: staged.digest.clone(),
        signer: None,
        tier: None,
        installed_at: install::now(),
        flags: Flags {
            allow_unsigned: unsigned,
            allow_removed: args.allow_removed,
            ..Flags::default()
        },
        held_update: None,
    };
    Ok(Fetched {
        staged,
        record,
        unsigned: unsigned.then(|| "index packages carry no publisher signature".to_owned()),
        repo: None,
    })
}

/// The pure-Python wheel of `dist` `version` on a PEP 503 page, resolved
/// against the page URL, with its `sha256`.
fn pick_wheel(
    page: &str,
    page_url: &str,
    dist: &str,
    version: &str,
) -> Result<(String, String), Error> {
    let prefix = format!("{}-{version}-", dist.replace(['-', '.'], "_")).to_ascii_lowercase();
    for (href, file) in anchors(page) {
        let lower = file.to_ascii_lowercase();
        let pure = lower.ends_with("-none-any.whl") && lower.contains("py3");
        if !lower.starts_with(&prefix) || !pure {
            continue;
        }
        let (link, fragment) = href.split_once('#').unwrap_or((&href, ""));
        let Some(sha256) = fragment.strip_prefix("sha256=") else {
            return Err(failed(format!("the index lists {file} without a sha256")));
        };
        let url = reqwest::Url::parse(page_url)
            .and_then(|base| base.join(link))
            .map_err(|e| failed(format!("bad link {link}: {e}")))?;
        return Ok((url.to_string(), sha256.to_owned()));
    }
    Err(failed(format!(
        "the index has no pure-Python wheel of {dist} {version}"
    )))
}

/// `(href, text)` of each `<a>` of an index page.
fn anchors(page: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut rest = page;
    while let Some(at) = rest.find("href=\"") {
        rest = &rest[at + 6..];
        let Some(end) = rest.find('"') else { break };
        let href = rest[..end].replace("&amp;", "&");
        rest = &rest[end..];
        let text = rest
            .find('>')
            .and_then(|open| {
                let body = &rest[open + 1..];
                body.find("</a>")
                    .map(|close| body[..close].trim().to_owned())
            })
            .unwrap_or_default();
        found.push((href, text));
    }
    found
}

/// Writes the plugin directory of `wheel` into `dir`.
fn materialize(
    wheel: &[u8],
    dir: &Path,
    (dist, version): (&str, &str),
    id: Option<&str>,
) -> Result<(), Error> {
    let bad = |why: String| failed(format!("{dist} {version}: {why}"));
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(wheel)).map_err(|e| bad(e.to_string()))?;
    let mut entry_points = String::new();
    let mut summary = String::new();
    for at in 0..archive.len() {
        let mut file = archive.by_index(at).map_err(|e| bad(e.to_string()))?;
        let Some(name) = file.enclosed_name() else {
            return Err(bad(format!("unsafe path {:?} in the wheel", file.name())));
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| bad(e.to_string()))?;
        let top = name
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().into_owned());
        let top = top.unwrap_or_default();
        if top.ends_with(".dist-info") {
            match name.file_name().and_then(|n| n.to_str()) {
                Some("entry_points.txt") => {
                    entry_points = String::from_utf8_lossy(&bytes).into_owned();
                }
                Some("METADATA") => summary = metadata_summary(&String::from_utf8_lossy(&bytes)),
                _ => {}
            }
            continue;
        }
        if std::path::Path::new(&top)
            .extension()
            .is_some_and(|e| e == "data")
            || file.is_dir()
        {
            continue;
        }
        let target = dir.join("site").join(&name);
        write(&target, &bytes)?;
    }
    let (plugin, target) = entry_point(&entry_points)
        .ok_or_else(|| bad("declares no `hermes_agent.plugins` entry point".to_owned()))?;
    write(
        &dir.join("plugin.yaml"),
        plugin_yaml(id.unwrap_or(&plugin), dist, version, &summary).as_bytes(),
    )?;
    write(
        &dir.join("__init__.py"),
        loader(dist, version, &plugin, &target).as_bytes(),
    )
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::io(&codes::PLUGINS_CATALOG, parent, &e))?;
    }
    std::fs::write(path, bytes).map_err(|e| Error::io(&codes::PLUGINS_CATALOG, path, &e))
}

fn metadata_summary(metadata: &str) -> String {
    metadata
        .lines()
        .find_map(|l| l.strip_prefix("Summary:"))
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

/// The first `name = module[:attr]` of a plugin entry-point group.
fn entry_point(text: &str) -> Option<(String, String)> {
    for group in GROUPS {
        let mut inside = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                inside = line == format!("[{group}]");
                continue;
            }
            if !inside {
                continue;
            }
            if let Some((name, target)) = line.split_once('=') {
                return Some((name.trim().to_owned(), target.trim().to_owned()));
            }
        }
    }
    None
}

/// A YAML double-quoted scalar.
fn quoted(text: &str) -> String {
    serde_json::Value::from(text).to_string()
}

fn plugin_yaml(plugin: &str, dist: &str, version: &str, summary: &str) -> String {
    format!(
        "# Generated by `prometheus plugins install pypi:{dist}=={version}`.\nname: {}\nversion: {}\ndescription: {}\npip_dependencies:\n  - {}\n",
        quoted(plugin),
        quoted(version),
        quoted(summary),
        quoted(&format!("{dist}=={version}")),
    )
}

/// The `__init__.py` that loads the entry point from `site/`.
fn loader(dist: &str, version: &str, plugin: &str, target: &str) -> String {
    let (module, attr) = target.split_once(':').unwrap_or((target, "register"));
    format!(
        "\"\"\"Entry point {plugin} = {target} of {dist} {version}, installed by Prometheus.\"\"\"\n\
         import os\nimport sys\nfrom importlib import import_module\n\n\
         _SITE = os.path.join(os.path.dirname(os.path.abspath(__file__)), \"site\")\n\
         if _SITE not in sys.path:\n    sys.path.insert(0, _SITE)\n\n\
         register = getattr(import_module({}), {})\n",
        quoted(module.trim()),
        quoted(attr.trim()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unpinned_package_is_refused() {
        for identifier in [
            "pypi:hermes-spotify",
            "pypi:hermes-spotify>=1",
            "pypi:x==1.*",
        ] {
            let err = parse(identifier).unwrap_err();
            assert_eq!(
                err.message(),
                "pypi plugins need an exact version: pypi:<dist>==<version>"
            );
        }
        assert_eq!(
            parse("pypi:hermes-spotify==1.2.0").unwrap(),
            ("hermes-spotify".to_owned(), "1.2.0".to_owned())
        );
    }

    #[test]
    fn the_plugin_entry_point_is_read() {
        let text = "[console_scripts]\nx = y:z\n\n[hermes_agent.plugins]\nspotify = hermes_spotify:register\n";
        assert_eq!(
            entry_point(text),
            Some(("spotify".to_owned(), "hermes_spotify:register".to_owned()))
        );
    }

    #[test]
    fn the_wheel_is_picked_from_the_index_page() {
        let page = "<a href=\"../files/hermes_spotify-1.2.0-py3-none-any.whl#sha256=ab\">hermes_spotify-1.2.0-py3-none-any.whl</a>\n<a href=\"../files/hermes_spotify-1.2.0.tar.gz#sha256=cd\">hermes_spotify-1.2.0.tar.gz</a>";
        let (url, sha) = pick_wheel(
            page,
            "http://127.0.0.1:1/hermes-spotify/",
            "hermes-spotify",
            "1.2.0",
        )
        .unwrap();
        assert_eq!(
            url,
            "http://127.0.0.1:1/files/hermes_spotify-1.2.0-py3-none-any.whl"
        );
        assert_eq!(sha, "ab");
    }
}

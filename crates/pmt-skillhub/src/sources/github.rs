//! GitHub repositories: direct `<owner>/<repo>/<path>` identifiers, taps
//! and the repositories behind skills.sh entries. A fetch resolves the ref
//! to a commit SHA first, so every file comes from that one commit and the
//! lockfile records it.

use std::collections::BTreeMap;

use pmt_core::error::{Error, codes};
use pmt_skills::layout::{self, SKILL_FILE};

use crate::bundle::{Bundle, Upstream, safe_path};
use crate::http::{self, get_bytes, get_json};
use crate::hub::Hub;
use crate::ident::Identifier;

/// The commit SHA `at` (default `HEAD`) of `owner/repo`.
///
/// # Errors
///
/// `skill.not_found` for an unknown repository or ref,
/// `skill.hub_unavailable`.
pub fn commit(hub: &Hub, repo: &str, at: Option<&str>) -> Result<String, Error> {
    let at = at.unwrap_or("HEAD");
    let url = format!("{}/repos/{repo}/commits/{at}", hub.endpoints.github_api);
    let value = get_json(hub.http.as_ref(), hub.github(url))?
        .ok_or_else(|| not_found(format!("GitHub repository '{repo}' (ref {at}) not found")))?;
    value["sha"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| http::unavailable(repo, "the commit answer has no sha"))
}

/// Every file path of `repo` at `sha`.
///
/// # Errors
///
/// As [`commit`].
pub fn tree(hub: &Hub, repo: &str, sha: &str) -> Result<Vec<String>, Error> {
    let url = format!(
        "{}/repos/{repo}/git/trees/{sha}?recursive=1",
        hub.endpoints.github_api
    );
    let value = get_json(hub.http.as_ref(), hub.github(url))?
        .ok_or_else(|| not_found(format!("GitHub tree {repo}@{sha} not found")))?;
    Ok(value["tree"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|i| i["type"] == "blob")
                .filter_map(|i| i["path"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
}

fn not_found(message: String) -> Error {
    Error::new(&codes::SKILL_NOT_FOUND, message)
}

/// Downloads the files below `dir` of `repo` at `sha` (excluded
/// directories skipped), relative to `dir`.
///
/// # Errors
///
/// `skill.hub_unavailable`, `skill.not_found` for a vanished file.
pub fn download(
    hub: &Hub,
    repo: &str,
    sha: &str,
    dir: &str,
    paths: &[String],
) -> Result<BTreeMap<String, Vec<u8>>, Error> {
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    let mut files = BTreeMap::new();
    for path in paths {
        let Some(rel) = path.strip_prefix(&prefix) else {
            continue;
        };
        if !safe_path(rel) {
            continue;
        }
        let url = format!("{}/{repo}/{sha}/{path}", hub.endpoints.github_raw);
        let mut request = http::Request::get(url);
        if let Some(token) = &hub.github_token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let bytes = get_bytes(hub.http.as_ref(), request)?
            .ok_or_else(|| not_found(format!("{repo}@{sha}: {path} vanished")))?;
        files.insert(rel.to_owned(), bytes);
    }
    Ok(files)
}

/// The skill directories below `base` of `repo` at `sha`: `(directory
/// name, path)`, skipping names starting with `.` or `_`.
#[must_use]
pub fn skill_dirs_below(paths: &[String], base: &str) -> Vec<(String, String)> {
    let base = base.trim_matches('/');
    let below: Vec<&str> = paths
        .iter()
        .filter_map(|p| {
            if base.is_empty() {
                Some(p.as_str())
            } else {
                p.strip_prefix(base)?.strip_prefix('/')
            }
        })
        .collect();
    layout::skill_dirs(below)
        .into_iter()
        .filter(|rel| !rel.is_empty() && !rel.contains('/'))
        .filter(|name| !name.starts_with('.') && !name.starts_with('_'))
        .map(|name| {
            let path = if base.is_empty() {
                name.clone()
            } else {
                format!("{base}/{name}")
            };
            (name, path)
        })
        .collect()
}

/// The candidate repository paths of skill path `path` of `repo`: the
/// path as given, under each tap's path, then under `skills/`.
fn candidates(hub: &Hub, repo: &str, path: &str) -> Vec<String> {
    let mut out = vec![path.to_owned()];
    for tap in hub
        .taps
        .iter()
        .filter(|t| t.repo.eq_ignore_ascii_case(repo))
    {
        out.push(tap.skill_path(path));
    }
    out.push(format!("skills/{path}"));
    out.dedup();
    out
}

/// Fetches skill `path` of `repo` at `at`.
///
/// # Errors
///
/// `skill.not_found` when no candidate directory holds a `SKILL.md`,
/// `skill.hub_unavailable`, `skill.invalid`.
pub fn fetch(
    hub: &Hub,
    id: Identifier,
    repo: &str,
    path: &str,
    at: Option<&str>,
) -> Result<Bundle, Error> {
    let sha = commit(hub, repo, at)?;
    let paths = tree(hub, repo, &sha)?;
    let dir = candidates(hub, repo, path)
        .into_iter()
        .find(|dir| paths.iter().any(|p| *p == format!("{dir}/{SKILL_FILE}")))
        .ok_or_else(|| not_found(format!("no skill at '{path}' in {repo}")))?;
    fetch_dir(hub, id, repo, &sha, &dir, &paths)
}

/// Fetches the skill directory `dir` of `repo` at commit `sha`.
///
/// # Errors
///
/// As [`fetch`].
pub fn fetch_dir(
    hub: &Hub,
    id: Identifier,
    repo: &str,
    sha: &str,
    dir: &str,
    paths: &[String],
) -> Result<Bundle, Error> {
    let files = download(hub, repo, sha, dir, paths)?;
    let mut bundle = Bundle::new(id, files, sha.to_owned(), hub.repo_trust(repo))?;
    if bundle.name.is_empty() {
        dir.rsplit('/')
            .next()
            .unwrap_or(dir)
            .clone_into(&mut bundle.name);
    }
    if bundle.category.is_none() {
        bundle.category = dir
            .rsplit('/')
            .nth(1)
            .filter(|c| *c != "skills" && pmt_skills::frontmatter::valid_name(c))
            .map(str::to_owned);
    }
    bundle.upstream = Upstream {
        repo_url: Some(format!("https://github.com/{repo}/tree/{sha}/{dir}")),
        ..Upstream::default()
    };
    Ok(bundle)
}

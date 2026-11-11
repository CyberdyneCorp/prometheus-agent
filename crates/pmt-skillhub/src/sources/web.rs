//! Skills served over plain HTTPS: a site's `/.well-known/skills/`
//! endpoint, and a direct `SKILL.md` URL (with only the support files it
//! references, from the same host). Both are `community` trust.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use pmt_core::error::{Error, codes};
use pmt_skills::layout::{SKILL_FILE, SUPPORT_DIRS};
use pmt_skills::lockfile::Trust;
use regex::Regex;

use crate::bundle::{Bundle, Upstream, safe_path};
use crate::http::{Request, get_bytes, get_json};
use crate::hub::Hub;
use crate::ident::Identifier;
use crate::sources::Listing;

/// The well-known path below a site.
pub const WELL_KNOWN: &str = "/.well-known/skills";

/// `(index URL, skill base URL, skill name)` of a well-known identifier.
fn well_known_parts(url: &str) -> Option<(String, String, Option<String>)> {
    if let Some((base, name)) = url.split_once("/index.json#") {
        return Some((
            format!("{base}/index.json"),
            base.to_owned(),
            Some(name.to_owned()),
        ));
    }
    let url = url.trim_end_matches("/SKILL.md");
    match url.split_once(&format!("{WELL_KNOWN}/")) {
        Some((site, name)) if !name.is_empty() && !name.contains('/') => {
            let base = format!("{site}{WELL_KNOWN}");
            Some((format!("{base}/index.json"), base, Some(name.to_owned())))
        }
        Some(_) => None,
        None => {
            let base = format!("{}{WELL_KNOWN}", url.trim_end_matches('/'));
            Some((format!("{base}/index.json"), base, None))
        }
    }
}

fn index(hub: &Hub, index_url: &str) -> Result<Vec<serde_json::Value>, Error> {
    let value = get_json(hub.http.as_ref(), Request::get(index_url))?.ok_or_else(|| {
        Error::new(
            &codes::SKILL_NOT_FOUND,
            format!("{index_url}: no skills index"),
        )
    })?;
    Ok(value["skills"].as_array().cloned().unwrap_or_default())
}

/// Fetches a well-known skill.
///
/// # Errors
///
/// `skill.not_found`, `skill.hub_unavailable`, `skill.invalid`.
pub fn fetch_well_known(hub: &Hub, url: &str) -> Result<Bundle, Error> {
    let (index_url, base, name) = well_known_parts(url).ok_or_else(|| {
        Error::new(
            &codes::SKILL_INVALID,
            format!("well-known:{url}: not a skill URL"),
        )
    })?;
    let name = name.ok_or_else(|| {
        Error::new(
            &codes::CLI_USAGE,
            format!("well-known:{url} names a site; add the skill: well-known:{base}/<name>"),
        )
    })?;
    let entries = index(hub, &index_url)?;
    let entry = entries
        .iter()
        .find(|e| e["name"] == name.as_str())
        .ok_or_else(|| {
            Error::new(
                &codes::SKILL_NOT_FOUND,
                format!("{index_url}: no skill '{name}'"),
            )
        })?;
    let listed: Vec<String> = entry["files"].as_array().map_or_else(
        || vec![SKILL_FILE.to_owned()],
        |f| {
            f.iter()
                .filter_map(|p| p.as_str().map(str::to_owned))
                .collect()
        },
    );
    let mut files = BTreeMap::new();
    for rel in listed {
        if !safe_path(&rel) {
            return Err(Error::new(
                &codes::SKILL_INVALID,
                format!("{index_url}: unsafe file path '{rel}'"),
            ));
        }
        let bytes = get_bytes(
            hub.http.as_ref(),
            Request::get(format!("{base}/{name}/{rel}")),
        )?
        .ok_or_else(|| {
            Error::new(
                &codes::SKILL_NOT_FOUND,
                format!("{base}/{name}/{rel}: not found"),
            )
        })?;
        files.insert(rel, bytes);
    }
    let id = Identifier::WellKnown {
        url: format!("{base}/{name}"),
    };
    let mut bundle = Bundle::new(id, files, index_url, Trust::Community)?;
    if bundle.name.is_empty() {
        bundle.name = name;
    }
    bundle.upstream = Upstream {
        repo_url: Some(base),
        ..Upstream::default()
    };
    Ok(bundle)
}

/// The skills a well-known endpoint lists.
///
/// # Errors
///
/// `skill.not_found`, `skill.hub_unavailable`.
pub fn search_well_known(hub: &Hub, site: &str) -> Result<Vec<Listing>, Error> {
    let Some((index_url, base, _)) = well_known_parts(site) else {
        return Ok(Vec::new());
    };
    Ok(index(hub, &index_url)?
        .iter()
        .filter_map(|e| {
            let name = e["name"].as_str()?;
            Some(Listing {
                identifier: format!("well-known:{base}/{name}"),
                name: name.to_owned(),
                description: e["description"].as_str().unwrap_or_default().to_owned(),
                source: "well-known".to_owned(),
                trust: Trust::Community,
                installs: None,
            })
        })
        .collect())
}

static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    let dirs = SUPPORT_DIRS.join("|");
    Regex::new(&format!(
        r"(?:^|[\s(\[`'\x22])((?:{dirs})/[A-Za-z0-9._/-]+)"
    ))
    .expect("reference pattern")
});

/// The support files a `SKILL.md` names (`references/x.md`...).
fn referenced(text: &str) -> Vec<String> {
    let mut out: Vec<String> = REFERENCE
        .captures_iter(text)
        .filter_map(|c| c.get(1))
        .map(|m| m.as_str().trim_end_matches(['.', ',', ')', '`']).to_owned())
        .filter(|p| safe_path(p))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Names a direct URL cannot be installed under.
const GENERIC: [&str; 4] = ["skill", "readme", "index", "unnamed-skill"];

/// The skill name of a direct URL: its frontmatter `name`, else the URL
/// slug (`.../<name>/SKILL.md` or `.../<name>.md`), else none.
fn url_name(bundle: &Bundle, url: &str) -> Option<String> {
    let usable = |n: &str| {
        let n = n.to_ascii_lowercase();
        pmt_skills::frontmatter::valid_name(&n)
            .then_some(n)
            .filter(|n| !GENERIC.contains(&n.as_str()))
    };
    if let Some(name) = usable(&bundle.name) {
        return Some(name);
    }
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let mut parts = path.rsplit('/');
    let file = parts.next()?;
    if file.eq_ignore_ascii_case(SKILL_FILE) {
        return parts.next().and_then(usable);
    }
    usable(
        file.strip_suffix(".md")
            .or_else(|| file.strip_suffix(".MD"))?,
    )
}

fn host(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap_or_default()
}

/// Fetches a direct `SKILL.md` URL; the bundle's name is empty when
/// neither the frontmatter nor the URL slug gives one.
///
/// # Errors
///
/// `skill.not_found`, `skill.hub_unavailable`, `skill.invalid`.
pub fn fetch_url(hub: &Hub, url: &str) -> Result<Bundle, Error> {
    let text = get_bytes(hub.http.as_ref(), Request::get(url))?
        .ok_or_else(|| Error::new(&codes::SKILL_NOT_FOUND, format!("{url}: not found")))?;
    let mut files = BTreeMap::new();
    let base = url.rsplit_once('/').map_or(url, |(b, _)| b).to_owned();
    for rel in referenced(&String::from_utf8_lossy(&text)) {
        let support = format!("{base}/{rel}");
        if host(&support) != host(url) {
            continue;
        }
        if let Ok(Some(bytes)) = get_bytes(hub.http.as_ref(), Request::get(&support)) {
            files.insert(rel, bytes);
        } else {
            tracing::warn!(url = %support, "referenced support file not fetched");
        }
    }
    files.insert(SKILL_FILE.to_owned(), text);
    let id = Identifier::Url {
        url: url.to_owned(),
    };
    let mut bundle = Bundle::new(id, files, url.to_owned(), Trust::Community)?;
    bundle.name = url_name(&bundle, url).unwrap_or_default();
    bundle.upstream.repo_url = Some(url.to_owned());
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn referenced_support_files_are_found() {
        let text =
            "See references/api.md and (scripts/run.sh), not ../etc or http://x/references/y.";
        assert_eq!(referenced(text), ["references/api.md", "scripts/run.sh"]);
    }

    #[test]
    fn well_known_identifiers_split_into_index_and_skill() {
        let (index, base, name) =
            well_known_parts("https://ex.com/.well-known/skills/deploy").unwrap();
        assert_eq!(index, "https://ex.com/.well-known/skills/index.json");
        assert_eq!(base, "https://ex.com/.well-known/skills");
        assert_eq!(name.as_deref(), Some("deploy"));
        assert_eq!(well_known_parts("https://ex.com").unwrap().2, None);
    }
}

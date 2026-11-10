//! The skill registries with their own APIs, all `community` trust:
//! ClawHub (versioned zip archives), LobeHub (system-prompt agents turned
//! into a `SKILL.md`) and browse.sh (site automation `SKILL.md` files).

use std::collections::BTreeMap;
use std::io::Read as _;

use pmt_core::error::{Error, codes};
use pmt_skills::layout::SKILL_FILE;
use pmt_skills::lockfile::Trust;
use serde_json::Value;

use crate::bundle::{Bundle, Upstream, safe_path};
use crate::http::{MAX_BYTES, Request, get_bytes, get_json};
use crate::hub::Hub;
use crate::ident::Identifier;
use crate::sources::{Listing, encode};

fn not_found(what: &str) -> Error {
    Error::new(&codes::SKILL_NOT_FOUND, format!("{what}: not found"))
}

/// `(slug, owner)` of `<owner>/<slug>` or `<slug>`.
fn clawhub_slug(slug: &str) -> (&str, Option<&str>) {
    match slug.split_once('/') {
        Some((owner, slug)) => (slug, Some(owner)),
        None => (slug, None),
    }
}

/// The ClawHub skill record (`{"skill": {...}, "latestVersion": ...}`
/// flattened).
fn clawhub_detail(hub: &Hub, slug: &str) -> Result<Value, Error> {
    let (name, owner) = clawhub_slug(slug);
    let mut url = format!("{}/skills/{name}", hub.endpoints.clawhub);
    if let Some(owner) = owner {
        url = format!("{url}?owner={}", encode(owner));
    }
    let value = get_json(hub.http.as_ref(), Request::get(url))?
        .ok_or_else(|| not_found(&format!("clawhub:{slug}")))?;
    let mut flat = value.get("skill").cloned().unwrap_or_else(|| value.clone());
    for key in ["latestVersion", "owner"] {
        if let (Some(v), Some(map)) = (value.get(key), flat.as_object_mut()) {
            map.entry(key).or_insert_with(|| v.clone());
        }
    }
    Ok(flat)
}

fn version_of(detail: &Value) -> Option<String> {
    match &detail["latestVersion"] {
        Value::String(v) => Some(v.clone()),
        Value::Object(o) => o.get("version").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

fn unzip(bytes: &[u8], what: &str) -> Result<BTreeMap<String, Vec<u8>>, Error> {
    let bad = |why: String| Error::new(&codes::SKILL_INVALID, format!("{what}: {why}"));
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| bad(e.to_string()))?;
    let mut files = BTreeMap::new();
    let mut total = 0usize;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|e| bad(e.to_string()))?;
        if entry.is_dir() {
            continue;
        }
        let name = entry
            .name()
            .map_err(|e| bad(e.to_string()))?
            .trim_start_matches("./")
            .to_owned();
        if !safe_path(&name) {
            return Err(bad(format!("unsafe path '{name}' in the archive")));
        }
        let mut content = Vec::new();
        entry
            .by_ref()
            .take((MAX_BYTES - total) as u64 + 1)
            .read_to_end(&mut content)
            .map_err(|e| bad(e.to_string()))?;
        total += content.len();
        if total > MAX_BYTES {
            return Err(bad(format!(
                "the archive unpacks to more than {MAX_BYTES} bytes"
            )));
        }
        files.insert(name, content);
    }
    // An archive whose files sit in one top directory is that directory.
    let top: Option<String> = files
        .keys()
        .next()
        .and_then(|k| k.split_once('/').map(|(t, _)| t.to_owned()));
    if let Some(top) = top.filter(|t| files.keys().all(|k| k.starts_with(&format!("{t}/")))) {
        files = files
            .into_iter()
            .map(|(k, v)| (k[top.len() + 1..].to_owned(), v))
            .collect();
    }
    Ok(files)
}

/// Fetches `clawhub:<slug>` (`at`: a version; default the latest).
///
/// # Errors
///
/// `skill.not_found`, `skill.hub_unavailable`, `skill.invalid`.
pub fn fetch_clawhub(hub: &Hub, slug: &str, at: Option<&str>) -> Result<Bundle, Error> {
    let detail = clawhub_detail(hub, slug)?;
    let version = at
        .map(str::to_owned)
        .or_else(|| version_of(&detail))
        .ok_or_else(|| not_found(&format!("clawhub:{slug} (no published version)")))?;
    let (name, owner) = clawhub_slug(slug);
    let mut url = format!(
        "{}/download?slug={}&version={}",
        hub.endpoints.clawhub,
        encode(name),
        encode(&version)
    );
    if let Some(owner) = owner {
        url = format!("{url}&owner={}", encode(owner));
    }
    let archive = get_bytes(hub.http.as_ref(), Request::get(url))?
        .ok_or_else(|| not_found(&format!("clawhub:{slug}@{version}")))?;
    let files = unzip(&archive, &format!("clawhub:{slug}"))?;
    let id = Identifier::ClawHub {
        slug: slug.to_owned(),
    };
    let mut bundle = Bundle::new(id, files, version, Trust::Community)?;
    if bundle.name.is_empty() {
        name.clone_into(&mut bundle.name);
    }
    bundle.upstream = Upstream {
        repo_url: Some(format!("https://clawhub.ai/skills/{name}")),
        installs: detail["stats"]["installs"]
            .as_u64()
            .or_else(|| detail["stats"]["downloads"].as_u64()),
        audit: detail["moderation"]["status"]
            .as_str()
            .or_else(|| detail["audit"].as_str())
            .map(str::to_owned),
    };
    Ok(bundle)
}

/// Searches ClawHub.
///
/// # Errors
///
/// `skill.hub_unavailable`.
pub fn search_clawhub(hub: &Hub, query: &str, limit: usize) -> Result<Vec<Listing>, Error> {
    let url = format!(
        "{}/skills?search={}&limit={limit}",
        hub.endpoints.clawhub,
        encode(query)
    );
    let value = get_json(hub.http.as_ref(), Request::get(url))?.unwrap_or_default();
    let items = value["items"]
        .as_array()
        .or_else(|| value["skills"].as_array())
        .or_else(|| value.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(items
        .iter()
        .filter_map(|item| {
            let item = item.get("skill").unwrap_or(item);
            let slug = item["slug"].as_str()?;
            Some(Listing {
                identifier: format!("clawhub:{slug}"),
                name: item["displayName"]
                    .as_str()
                    .or_else(|| item["name"].as_str())
                    .unwrap_or(slug)
                    .to_owned(),
                description: item["summary"]
                    .as_str()
                    .or_else(|| item["description"].as_str())
                    .unwrap_or_default()
                    .to_owned(),
                source: "clawhub".to_owned(),
                trust: Trust::Community,
                installs: item["stats"]["installs"].as_u64(),
            })
        })
        .collect())
}

/// The `SKILL.md` of a LobeHub agent.
fn lobehub_skill_md(slug: &str, agent: &Value) -> String {
    let meta = agent.get("meta").unwrap_or(agent);
    let title = meta["title"].as_str().unwrap_or(slug);
    let description = meta["description"]
        .as_str()
        .unwrap_or(title)
        .replace('\n', " ");
    let description: String = description.chars().take(500).collect();
    let role = agent["config"]["systemRole"]
        .as_str()
        .unwrap_or("(No system role defined)");
    format!(
        "---\nname: {slug}\ndescription: {}\nmetadata:\n  prometheus:\n    source: lobehub\n---\n\n# {title}\n\n{description}\n\n## Instructions\n\n{role}\n",
        serde_json::to_string(&description).unwrap_or_default()
    )
}

/// Fetches `lobehub:<slug>`.
///
/// # Errors
///
/// `skill.not_found`, `skill.hub_unavailable`.
pub fn fetch_lobehub(hub: &Hub, slug: &str) -> Result<Bundle, Error> {
    let url = format!("{}/{slug}.json", hub.endpoints.lobehub);
    let agent = get_json(hub.http.as_ref(), Request::get(&url))?
        .ok_or_else(|| not_found(&format!("lobehub:{slug}")))?;
    let name = slug.rsplit('/').next().unwrap_or(slug).to_ascii_lowercase();
    let mut files = BTreeMap::new();
    files.insert(
        SKILL_FILE.to_owned(),
        lobehub_skill_md(&name, &agent).into_bytes(),
    );
    let id = Identifier::LobeHub {
        slug: slug.to_owned(),
    };
    let mut bundle = Bundle::new(id, files, url, Trust::Community)?;
    if bundle.name.is_empty() {
        bundle.name = name;
    }
    bundle.upstream.repo_url = Some(format!("https://lobehub.com/agents/{slug}"));
    Ok(bundle)
}

/// Searches the LobeHub index.
///
/// # Errors
///
/// `skill.hub_unavailable`.
pub fn search_lobehub(hub: &Hub, query: &str, limit: usize) -> Result<Vec<Listing>, Error> {
    let url = format!("{}/index.json", hub.endpoints.lobehub);
    let value = get_json(hub.http.as_ref(), Request::get(url))?.unwrap_or_default();
    let agents = value["agents"]
        .as_array()
        .or_else(|| value.as_array())
        .cloned()
        .unwrap_or_default();
    let query = query.to_lowercase();
    Ok(agents
        .iter()
        .filter_map(|agent| {
            let id = agent["identifier"].as_str()?;
            let meta = agent.get("meta").unwrap_or(agent);
            let title = meta["title"].as_str().unwrap_or(id);
            let description = meta["description"].as_str().unwrap_or_default();
            let hay = format!("{id} {title} {description}").to_lowercase();
            hay.contains(&query).then(|| Listing {
                identifier: format!("lobehub:{id}"),
                name: id.to_owned(),
                description: description.to_owned(),
                source: "lobehub".to_owned(),
                trust: Trust::Community,
                installs: None,
            })
        })
        .take(limit)
        .collect())
}

/// Fetches `browse-sh:<slug>`.
///
/// # Errors
///
/// `skill.not_found`, `skill.hub_unavailable`.
pub fn fetch_browse_sh(hub: &Hub, slug: &str) -> Result<Bundle, Error> {
    let url = format!("{}/api/skills/{slug}", hub.endpoints.browse_sh);
    let detail = get_json(hub.http.as_ref(), Request::get(url))?
        .ok_or_else(|| not_found(&format!("browse-sh:{slug}")))?;
    let md_url = detail["skillMdUrl"]
        .as_str()
        .filter(|u| u.starts_with("https://") || u.starts_with("http://127.0.0.1"))
        .ok_or_else(|| not_found(&format!("browse-sh:{slug} (no SKILL.md URL)")))?
        .to_owned();
    let text =
        get_bytes(hub.http.as_ref(), Request::get(&md_url))?.ok_or_else(|| not_found(&md_url))?;
    let mut files = BTreeMap::new();
    files.insert(SKILL_FILE.to_owned(), text);
    let id = Identifier::BrowseSh {
        slug: slug.to_owned(),
    };
    let mut bundle = Bundle::new(id, files, md_url, Trust::Community)?;
    if bundle.name.is_empty() {
        bundle.name = slug.rsplit('/').next().unwrap_or(slug).to_ascii_lowercase();
    }
    bundle.upstream = Upstream {
        repo_url: detail["sourceUrl"].as_str().map(str::to_owned),
        installs: detail["installCount"].as_u64(),
        audit: None,
    };
    Ok(bundle)
}

/// Searches the browse.sh catalog.
///
/// # Errors
///
/// `skill.hub_unavailable`.
pub fn search_browse_sh(hub: &Hub, query: &str, limit: usize) -> Result<Vec<Listing>, Error> {
    let url = format!("{}/api/skills", hub.endpoints.browse_sh);
    let value = get_json(hub.http.as_ref(), Request::get(url))?.unwrap_or_default();
    let query = query.to_lowercase();
    Ok(value["skills"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|item| {
            let slug = item["slug"].as_str()?;
            let description = item["description"].as_str().unwrap_or_default();
            let hay = format!(
                "{slug} {} {description}",
                item["name"].as_str().unwrap_or_default()
            )
            .to_lowercase();
            hay.contains(&query).then(|| Listing {
                identifier: format!("browse-sh:{slug}"),
                name: item["name"].as_str().unwrap_or(slug).to_owned(),
                description: description.to_owned(),
                source: "browse-sh".to_owned(),
                trust: Trust::Community,
                installs: item["installCount"].as_u64(),
            })
        })
        .take(limit)
        .collect())
}

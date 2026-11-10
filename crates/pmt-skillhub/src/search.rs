//! Hub discovery (spec: skills, Hub discovery, inspection, updates and
//! audit): `browse` lists the official catalog first, then the central
//! index; `search` answers from the official catalog, the cached central
//! index and the profile's taps, and only when they have no match spends
//! at most [`LIVE_BUDGET`] on live queries to skills.sh, ClawHub, LobeHub
//! and well-known endpoints, in parallel.

use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use pmt_core::error::Error;
use pmt_core::private_fs;
use pmt_skills::lockfile::Trust;

use crate::http::{Request, get_json};
use crate::hub::Hub;
use crate::sources::{Listing, github, official, registries, skills_sh, web};

/// The central index is refetched after this long.
pub const INDEX_TTL: Duration = Duration::from_hours(6);
/// The most time live queries may take together.
pub const LIVE_BUDGET: Duration = Duration::from_secs(8);
/// Results per live source.
const LIVE_LIMIT: usize = 10;

fn matches(listing: &Listing, terms: &[String]) -> bool {
    let hay = format!(
        "{} {} {}",
        listing.identifier, listing.name, listing.description
    )
    .to_lowercase();
    terms.iter().all(|t| hay.contains(t.as_str()))
}

fn terms(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

fn source_ok(listing: &Listing, source: Option<&str>) -> bool {
    source.is_none_or(|s| listing.source == s)
}

/// The central index: fresh from the cache, else fetched (and cached),
/// else the stale cache; empty when none is available.
#[must_use]
pub fn index(hub: &Hub) -> Vec<Listing> {
    let cache = hub.cache_dir().join("index.json");
    let fresh = std::fs::metadata(&cache)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < INDEX_TTL);
    let cached = || {
        std::fs::read(&cache)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
    };
    let value: Option<serde_json::Value> = if fresh {
        cached()
    } else {
        match get_json(hub.http.as_ref(), Request::get(&hub.endpoints.index)) {
            Ok(Some(value)) => {
                let _ = private_fs::create_dir(&hub.cache_dir());
                let _ = private_fs::write_atomic(&cache, value.to_string().as_bytes());
                Some(value)
            }
            _ => cached(),
        }
    };
    value
        .and_then(|v| v["skills"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|item| index_listing(hub, item))
        .collect()
}

fn index_listing(hub: &Hub, item: &serde_json::Value) -> Option<Listing> {
    let identifier = item["identifier"].as_str()?.to_owned();
    let id = crate::ident::Identifier::parse(&identifier).ok()?;
    let trust = match id.repository() {
        Some(repo) => hub.repo_trust(&repo),
        None if id.source() == "official" => Trust::Builtin,
        None => Trust::Community,
    };
    Some(Listing {
        name: item["name"].as_str().unwrap_or_default().to_owned(),
        description: item["description"].as_str().unwrap_or_default().to_owned(),
        source: id.source().to_owned(),
        trust,
        installs: item["installs"].as_u64(),
        identifier,
    })
}

/// The skills of every tap of the profile.
fn tap_listings(hub: &Hub) -> Vec<Listing> {
    let mut out = Vec::new();
    for tap in &hub.taps {
        let Ok(sha) = github::commit(hub, &tap.repo, None) else {
            continue;
        };
        let Ok(paths) = github::tree(hub, &tap.repo, &sha) else {
            continue;
        };
        for (name, _) in github::skill_dirs_below(&paths, &tap.path) {
            out.push(Listing {
                identifier: format!("{}/{name}", tap.repo),
                description: format!("from tap {}", tap.repo),
                name,
                source: "github".to_owned(),
                trust: hub.repo_trust(&tap.repo),
                installs: None,
            });
        }
    }
    out
}

/// `browse [--source S]`: the official catalog first, then the index and
/// the taps.
#[must_use]
pub fn browse(hub: &Hub, source: Option<&str>) -> Vec<Listing> {
    let mut out: Vec<Listing> = official::listings(hub);
    if source != Some("official") {
        out.extend(index(hub));
        out.extend(tap_listings(hub));
    }
    out.retain(|l| source_ok(l, source));
    dedup(out)
}

fn dedup(listings: Vec<Listing>) -> Vec<Listing> {
    let mut seen = std::collections::BTreeSet::new();
    listings
        .into_iter()
        .filter(|l| seen.insert(l.identifier.clone()))
        .collect()
}

/// `search <query> [--source S]`.
#[must_use]
pub fn search(hub: &Hub, query: &str, source: Option<&str>) -> Vec<Listing> {
    let terms = terms(query);
    let local: Vec<Listing> = browse(hub, source)
        .into_iter()
        .filter(|l| matches(l, &terms))
        .collect();
    if !local.is_empty() {
        return local;
    }
    let live = live(hub, query, source)
        .into_iter()
        .filter(|l| source_ok(l, source))
        .collect();
    dedup(live)
}

type LiveQuery = fn(&Hub, &str) -> Result<Vec<Listing>, Error>;

const LIVE: [(&str, LiveQuery); 4] = [
    ("skills-sh", |hub, q| skills_sh::search(hub, q, LIVE_LIMIT)),
    ("clawhub", |hub, q| {
        registries::search_clawhub(hub, q, LIVE_LIMIT)
    }),
    ("lobehub", |hub, q| {
        registries::search_lobehub(hub, q, LIVE_LIMIT)
    }),
    ("well-known", |hub, q| {
        if q.starts_with("https://") {
            web::search_well_known(hub, q)
        } else {
            Ok(Vec::new())
        }
    }),
];

/// The live queries, in parallel, cut off at [`LIVE_BUDGET`]: a source
/// still answering then is left behind (its thread ends on its own
/// request timeout).
fn live(hub: &Hub, query: &str, source: Option<&str>) -> Vec<Listing> {
    let deadline = Instant::now() + LIVE_BUDGET;
    let (tx, rx) = mpsc::channel();
    let mut pending = 0;
    for (name, run) in LIVE {
        if source.is_some_and(|s| s != name) {
            continue;
        }
        let (tx, hub, query) = (tx.clone(), hub.clone(), query.to_owned());
        pending += 1;
        std::thread::spawn(move || {
            let _ = tx.send(run(&hub, &query).unwrap_or_default());
        });
    }
    drop(tx);
    let mut out = Vec::new();
    for _ in 0..pending {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(found) => out.extend(found),
            Err(_) => break,
        }
    }
    out
}

//! The plugin catalog and its kill list (spec: plugin-system, Plugin
//! catalog; Catalog kill list).
//!
//! A catalog is a JSON document of entries (`id`, `repo` (`https://`
//! only), `ref` (a full 40-hex commit SHA or `sha256:<package digest>`),
//! `tier`, `category`, `capabilities_summary`, `publisher_key`, and for a
//! `sha256:` ref the `package` URL it downloads from) and a `removed` list
//! (`id` and/or `repo`, `reason`). One is embedded in the binary; the live
//! one from `plugins.catalog_url` is accepted only when it is at most
//! 2 MiB and its signature (`<url>.minisig`) verifies against the release
//! keys embedded in the binary, and is cached under the profile's
//! `cache/plugins/` for 6 hours. On any failure the embedded catalog is
//! used. Invalid entries are skipped with a WARN.
//!
//! The kill list is the union of the embedded and the cached live
//! `removed` lists, matched by id or by canonical repository
//! (host/owner/repo, ignoring case, scheme, `www.` and `.git`).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use pmt_core::error::{Error, codes};
use serde::Deserialize;
use serde_json::Value;

use crate::manifest::check_id;

/// The catalog compiled into this binary.
pub const EMBEDDED: &str = include_str!("../catalog/catalog.json");
/// How long a fetched live catalog is used before it is fetched again.
pub const CACHE_TTL: Duration = Duration::from_hours(6);
/// The largest live catalog accepted.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;
/// The cached catalog under a profile's `cache/`.
pub const CACHE_FILE: &str = "cache/plugins/catalog.json";
/// The tiers an entry may have.
pub const TIERS: &[&str] = &["official", "verified", "community"];

/// One catalog entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The plugin id.
    pub id: String,
    /// Its repository (`https://`).
    pub repo: String,
    /// A 40-hex commit SHA or `sha256:<64 hex>` package digest.
    pub r#ref: String,
    /// `official`, `verified` or `community`.
    pub tier: String,
    /// A grouping for `plugins browse`.
    pub category: String,
    /// The capabilities it asks for, in a line.
    pub capabilities_summary: String,
    /// The minisign public key (base64) its packages are signed with.
    pub publisher_key: String,
    /// The package URL of a `sha256:` ref.
    pub package: Option<String>,
    /// One line for listings.
    pub description: String,
    /// The version the ref holds, when the catalog says.
    pub version: Option<String>,
}

impl Entry {
    /// Whether the ref pins a package digest (else a commit).
    #[must_use]
    pub fn is_package(&self) -> bool {
        self.r#ref.starts_with("sha256:")
    }
}

/// One kill-list entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    /// A blocked plugin id.
    pub id: Option<String>,
    /// A blocked repository (canonical form).
    pub repo: Option<String>,
    /// Why.
    pub reason: String,
}

impl Removed {
    /// The refusal for plugin `id`.
    #[must_use]
    pub fn refusal(&self, id: &str) -> Error {
        Error::new(
            &codes::PLUGINS_REMOVED,
            format!(
                "Plugin '{id}' was removed from the catalog and is blocked: {}",
                self.reason
            ),
        )
    }
}

/// A parsed catalog.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    /// Its entries.
    pub entries: Vec<Entry>,
    /// Its kill list.
    pub removed: Vec<Removed>,
}

/// Verifies a detached minisign signature of catalog bytes against the
/// keys embedded in the binary; answers the signing key id.
pub type Verify<'a> = &'a dyn Fn(&[u8], &str) -> Result<String, String>;

#[derive(Deserialize)]
struct RawCatalog {
    #[serde(default)]
    plugins: Vec<Value>,
    #[serde(default)]
    removed: Vec<Value>,
}

impl Catalog {
    /// Parses catalog text; invalid entries are skipped with a WARN.
    ///
    /// # Errors
    ///
    /// `plugins.catalog` when the text is not a catalog document.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let raw: RawCatalog = serde_json::from_str(text).map_err(|e| {
            Error::new(
                &codes::PLUGINS_CATALOG,
                format!("the plugin catalog is not valid JSON: {e}"),
            )
        })?;
        let mut catalog = Self::default();
        for value in raw.plugins {
            match entry(&value) {
                Ok(entry) => catalog.entries.push(entry),
                Err(why) => tracing::warn!("plugin catalog entry skipped: {why}"),
            }
        }
        for value in raw.removed {
            match removed(&value) {
                Ok(removed) => catalog.removed.push(removed),
                Err(why) => tracing::warn!("plugin catalog removal skipped: {why}"),
            }
        }
        Ok(catalog)
    }

    /// The catalog embedded in the binary.
    ///
    /// # Panics
    ///
    /// Never for the committed file (unit-tested).
    #[must_use]
    pub fn embedded() -> Self {
        Self::parse(EMBEDDED).expect("the embedded plugin catalog parses")
    }

    /// The entry of `id`.
    #[must_use]
    pub fn find(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Entries whose id, category, description or capabilities mention
    /// `query` (case-insensitive), by id.
    #[must_use]
    pub fn search(&self, query: &str) -> Vec<&Entry> {
        let query = query.to_lowercase();
        let mut found: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|e| {
                [&e.id, &e.category, &e.description, &e.capabilities_summary]
                    .iter()
                    .any(|field| field.to_lowercase().contains(&query))
            })
            .collect();
        found.sort_by(|a, b| a.id.cmp(&b.id));
        found
    }

    /// The kill-list entry blocking plugin `id` from repository `repo`.
    #[must_use]
    pub fn blocked(&self, id: &str, repo: Option<&str>) -> Option<&Removed> {
        let repo = repo.map(canonical_repo);
        self.removed
            .iter()
            .find(|r| r.id.as_deref() == Some(id) || (r.repo.is_some() && r.repo == repo))
    }

    /// The entries of `live`, else the embedded ones, with the kill list
    /// of both.
    #[must_use]
    pub fn merged(live: Option<Self>) -> Self {
        let embedded = Self::embedded();
        let Some(live) = live else {
            return embedded;
        };
        let mut removed = embedded.removed;
        for entry in live.removed {
            if !removed.contains(&entry) {
                removed.push(entry);
            }
        }
        Self {
            entries: live.entries,
            removed,
        }
    }
}

/// `host/owner/repo` of a repository URL, lower-cased, without scheme,
/// `www.`, `.git` or a trailing `/`.
#[must_use]
pub fn canonical_repo(url: &str) -> String {
    let lower = url.trim().to_lowercase();
    let rest = lower.split_once("://").map_or(lower.as_str(), |(_, r)| r);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let rest = rest.trim_end_matches('/');
    rest.strip_suffix(".git").unwrap_or(rest).to_owned()
}

/// Whether `text` is a full 40-hex commit SHA.
#[must_use]
pub fn is_commit(text: &str) -> bool {
    text.len() == 40 && text.chars().all(|c| c.is_ascii_hexdigit())
}

/// Whether `text` is `sha256:<64 hex>`.
#[must_use]
pub fn is_digest(text: &str) -> bool {
    text.strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Whether a package may be downloaded from `url`: `https://`, or in a
/// debug build a loopback `http://` server (tests).
#[must_use]
pub fn fetchable(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    cfg!(debug_assertions)
        && ["http://127.0.0.1", "http://localhost", "http://[::1]"]
            .iter()
            .any(|prefix| url.starts_with(prefix))
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing '{key}'"))
}

fn entry(value: &Value) -> Result<Entry, String> {
    let id = text(value, "id")?;
    check_id(id).map_err(|e| e.message().to_owned())?;
    let named = |why: String| format!("'{id}': {why}");
    let repo = text(value, "repo").map_err(named)?;
    if !repo.starts_with("https://") {
        return Err(named(format!("repo '{repo}' is not https://")));
    }
    let r#ref = text(value, "ref").map_err(named)?;
    if !is_commit(r#ref) && !is_digest(r#ref) {
        let pinned = r#ref;
        return Err(named(format!(
            "ref '{pinned}' is neither a full commit SHA nor sha256:<digest>"
        )));
    }
    let tier = text(value, "tier").map_err(named)?;
    if !TIERS.contains(&tier) {
        return Err(named(format!("unknown tier '{tier}'")));
    }
    let publisher_key = text(value, "publisher_key").map_err(named)?;
    minisign_verify::PublicKey::from_base64(publisher_key)
        .map_err(|_| named("publisher_key is not a minisign public key".to_owned()))?;
    let package = value
        .get("package")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if is_digest(r#ref) && !package.as_deref().is_some_and(fetchable) {
        return Err(named(
            "a sha256 ref needs an https:// package URL".to_owned(),
        ));
    }
    let optional = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    Ok(Entry {
        id: id.to_owned(),
        repo: repo.to_owned(),
        r#ref: r#ref.to_owned(),
        tier: tier.to_owned(),
        category: optional("category").unwrap_or_default(),
        capabilities_summary: optional("capabilities_summary").unwrap_or_default(),
        publisher_key: publisher_key.to_owned(),
        package,
        description: optional("description").unwrap_or_default(),
        version: optional("version"),
    })
}

fn removed(value: &Value) -> Result<Removed, String> {
    let id = value.get("id").and_then(Value::as_str).map(str::to_owned);
    let repo = value
        .get("repo")
        .and_then(Value::as_str)
        .map(canonical_repo);
    if id.is_none() && repo.is_none() {
        return Err("a removal names neither an id nor a repo".to_owned());
    }
    Ok(Removed {
        id,
        repo,
        reason: text(value, "reason")?.to_owned(),
    })
}

/// Checks and parses a fetched live catalog.
///
/// # Errors
///
/// Why it is refused: too large, a signature that does not verify, or not
/// a catalog.
pub fn accept(bytes: &[u8], signature: &str, verify: Verify<'_>) -> Result<Catalog, String> {
    if bytes.len() > MAX_BYTES {
        return Err(format!(
            "the catalog is {} bytes, more than {MAX_BYTES}",
            bytes.len()
        ));
    }
    verify(bytes, signature).map_err(|e| format!("signature verification failed: {e}"))?;
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    Catalog::parse(text).map_err(|e| e.message().to_owned())
}

/// The cached live catalog of a profile.
#[derive(Debug, Clone)]
pub struct Cache {
    file: PathBuf,
}

impl Cache {
    /// The cache of the profile at `profile_dir`.
    #[must_use]
    pub fn of(profile_dir: &Path) -> Self {
        Self {
            file: profile_dir.join(CACHE_FILE),
        }
    }

    fn signature_file(&self) -> PathBuf {
        self.file.with_extension("json.minisig")
    }

    /// The cached catalog, re-verified, with its age; `None` when there is
    /// none or it no longer verifies.
    #[must_use]
    pub fn read(&self, verify: Verify<'_>) -> Option<(Catalog, Duration)> {
        let bytes = std::fs::read(&self.file).ok()?;
        let signature = std::fs::read_to_string(self.signature_file()).ok()?;
        let age = std::fs::metadata(&self.file)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .unwrap_or(Duration::MAX);
        match accept(&bytes, &signature, verify) {
            Ok(catalog) => Some((catalog, age)),
            Err(why) => {
                tracing::warn!(file = %self.file.display(), "cached plugin catalog ignored: {why}");
                None
            }
        }
    }

    /// Stores a verified catalog and its signature, each atomically.
    ///
    /// # Errors
    ///
    /// `plugins.catalog` when the cache cannot be written.
    pub fn write(&self, bytes: &[u8], signature: &str) -> Result<(), Error> {
        let dir = self.file.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| Error::io(&codes::PLUGINS_CATALOG, dir, &e))?;
        atomic_write(&self.signature_file(), signature.as_bytes())?;
        atomic_write(&self.file, bytes)
    }

    /// The kill list in force without a network call: the embedded one and
    /// the cached live one.
    #[must_use]
    pub fn kill_list(&self, verify: Verify<'_>) -> Vec<Removed> {
        Catalog::merged(self.read(verify).map(|(c, _)| c)).removed
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes)
        .and_then(|()| std::fs::rename(&temporary, path))
        .map_err(|e| Error::io(&codes::PLUGINS_CATALOG, path, &e))
}

#[cfg(test)]
mod tests;

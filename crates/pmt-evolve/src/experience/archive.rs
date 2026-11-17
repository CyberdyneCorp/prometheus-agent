//! Moving a domain between profiles (spec: experience-library, Export
//! and import between profiles). An archive is a zstd-compressed tar of
//! `manifest.toml` (`domain`) and `entries/<entry_id>/{EXPERIENCE.md,
//! entry.toml, harness/…}`. Import is all or nothing: every entry's
//! harness must validate and pass the leak globs, the verdict tripwire
//! and the secret scanner before anything is written; accepted entries
//! get new ids, `status = "active"`, `pinned = false` and
//! `imported_from`.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::Path;

use pmt_config::harness::Tree;
use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

use super::entry::{self, EntryMeta, Status};
use super::library::{Library, NewEntry};
use super::record::{self, Settings};
use crate::harness::{Rules, validate};

/// The most bytes an archive may unpack to.
pub const MAX_UNPACKED: u64 = 256 * 1024 * 1024;

/// `manifest.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    domain: String,
    exported_at: String,
}

fn failed(path: &Path, why: impl std::fmt::Display) -> Error {
    Error::new(&codes::EXPERIENCE_IO, format!("{}: {why}", path.display()))
}

fn rejected(message: impl Into<String>) -> Error {
    Error::new(&codes::EXPERIENCE_IMPORT_REJECTED, message)
}

/// Writes every entry of `domain` to `file`; the number exported.
///
/// # Errors
///
/// `experience.io`.
pub fn export(profile_dir: &Path, domain: &str, file: &Path) -> Result<usize, Error> {
    let library = Library::of(profile_dir);
    let entries = library.entries(domain);
    let mut builder = tar::Builder::new(Vec::new());
    let mut add = |rel: &str, bytes: &[u8]| -> Result<(), Error> {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        builder
            .append_data(&mut header, rel, bytes)
            .map_err(|e| failed(file, e))
    };
    let manifest = Manifest {
        domain: domain.to_owned(),
        exported_at: jiff::Timestamp::now().to_string(),
    };
    let text = toml::to_string(&manifest).map_err(|e| failed(file, e))?;
    add("manifest.toml", text.as_bytes())?;
    for meta in &entries {
        let base = format!("entries/{}", meta.entry_id);
        let meta_text = toml::to_string(meta).map_err(|e| failed(file, e))?;
        add(&format!("{base}/entry.toml"), meta_text.as_bytes())?;
        add(
            &format!("{base}/EXPERIENCE.md"),
            library.document(meta).as_bytes(),
        )?;
        for (path, text) in &library.tree(meta)?.files {
            add(&format!("{base}/harness/{path}"), text.as_bytes())?;
        }
    }
    let tarball = builder.into_inner().map_err(|e| failed(file, e))?;
    let packed = zstd::encode_all(tarball.as_slice(), 3).map_err(|e| failed(file, e))?;
    pmt_core::private_fs::write_atomic(file, &packed).map_err(|e| failed(file, e))?;
    Ok(entries.len())
}

/// One entry read from an archive.
#[derive(Debug, Default)]
struct Packed {
    meta: Option<String>,
    document: String,
    harness: BTreeMap<String, String>,
}

fn unpack(file: &Path) -> Result<(Option<Manifest>, BTreeMap<String, Packed>), Error> {
    let reader = std::fs::File::open(file).map_err(|e| failed(file, e))?;
    let decoder = zstd::stream::read::Decoder::new(reader).map_err(|e| failed(file, e))?;
    let mut tarball = Vec::new();
    decoder
        .take(MAX_UNPACKED + 1)
        .read_to_end(&mut tarball)
        .map_err(|e| {
            rejected(format!(
                "{}: not an experience archive ({e})",
                file.display()
            ))
        })?;
    if tarball.len() as u64 > MAX_UNPACKED {
        return Err(rejected(format!(
            "{} unpacks to more than {MAX_UNPACKED} bytes",
            file.display()
        )));
    }
    let mut manifest = None;
    let mut entries: BTreeMap<String, Packed> = BTreeMap::new();
    let mut archive = tar::Archive::new(tarball.as_slice());
    for item in archive.entries().map_err(|e| failed(file, e))? {
        let mut item = item.map_err(|e| failed(file, e))?;
        if !item.header().entry_type().is_file() {
            continue;
        }
        let rel = item
            .path()
            .map_err(|e| failed(file, e))?
            .to_string_lossy()
            .replace('\\', "/");
        if rel.starts_with('/') || rel.split('/').any(|p| p == ".." || p.is_empty()) {
            return Err(rejected(format!(
                "unsafe path '{rel}' in {}",
                file.display()
            )));
        }
        let mut text = String::new();
        item.read_to_string(&mut text)
            .map_err(|_| rejected(format!("{rel} is not UTF-8 text")))?;
        place(&rel, text, &mut manifest, &mut entries)?;
    }
    Ok((manifest, entries))
}

fn place(
    rel: &str,
    text: String,
    manifest: &mut Option<Manifest>,
    entries: &mut BTreeMap<String, Packed>,
) -> Result<(), Error> {
    if rel == "manifest.toml" {
        *manifest = toml::from_str(&text).ok();
        return Ok(());
    }
    let mut parts = rel.splitn(3, '/');
    let (Some("entries"), Some(id), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(rejected(format!("unexpected file '{rel}' in the archive")));
    };
    let packed = entries.entry(id.to_owned()).or_default();
    match rest {
        "entry.toml" => packed.meta = Some(text),
        "EXPERIENCE.md" => packed.document = text,
        _ => match rest.strip_prefix("harness/") {
            Some(path) => {
                packed.harness.insert(path.to_owned(), text);
            }
            None => return Err(rejected(format!("unexpected file '{rel}' in the archive"))),
        },
    }
    Ok(())
}

/// What an import is checked with.
#[derive(Debug, Clone, Copy)]
pub struct ImportRules<'a> {
    /// Harness validation.
    pub rules: &'a Rules,
    /// Leak globs, secrets and the cap.
    pub settings: &'a Settings,
}

/// Imports every entry of `file` into `domain` (default: the archive's);
/// the new entries.
///
/// # Errors
///
/// `experience.import_rejected` (65) naming the first entry that fails,
/// with nothing written; `experience.invalid_domain`; `experience.io`.
pub fn import(
    profile_dir: &Path,
    file: &Path,
    domain: Option<&str>,
    checks: ImportRules<'_>,
) -> Result<Vec<EntryMeta>, Error> {
    let (manifest, packed) = unpack(file)?;
    let domain = match domain.or(manifest.as_ref().map(|m| m.domain.as_str())) {
        Some(name) => super::domain::parse(name)?,
        None => return Err(rejected(format!("{} names no domain", file.display()))),
    };
    let from = file.display().to_string();
    let mut accepted = Vec::new();
    for (id, entry) in packed {
        accepted.push(checked(&id, entry, &domain, &from, checks)?);
    }
    let library = Library::of(profile_dir);
    let mut written = Vec::new();
    for new in &accepted {
        written.push(library.write(new, checks.settings.cap)?.meta);
    }
    Ok(written)
}

fn checked(
    id: &str,
    entry: Packed,
    domain: &str,
    from: &str,
    checks: ImportRules<'_>,
) -> Result<NewEntry, Error> {
    let refuse = |why: String| rejected(format!("entry {id}: {why}"));
    let meta_text = entry
        .meta
        .ok_or_else(|| refuse("missing entry.toml".into()))?;
    let mut meta: EntryMeta =
        toml::from_str(&meta_text).map_err(|e| refuse(format!("entry.toml: {}", e.message())))?;
    let mut tree = Tree {
        files: entry.harness,
    };
    validate(&mut tree, checks.rules)
        .map_err(|errors| refuse(format!("harness does not validate: {}", errors.join("; "))))?;
    if let Err((screen, detail)) = record::screen(checks.settings, &tree, &entry.document, "") {
        return Err(refuse(format!("{screen}: {detail}")));
    }
    let now = jiff::Timestamp::now().to_string();
    meta.entry_id = entry::new_id(&meta.title, &now);
    domain.clone_into(&mut meta.domain);
    meta.created_at = now;
    meta.status = Status::Active;
    meta.pinned = false;
    meta.times_shown = 0;
    meta.proposal_id = String::new();
    meta.imported_from = Some(from.to_owned());
    Ok(NewEntry {
        meta,
        document: entry.document,
        tree,
    })
}

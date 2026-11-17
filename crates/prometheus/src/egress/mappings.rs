//! The placeholders of the profile's egress proxy (spec: security-
//! boundaries, Egress credential source and placeholder rotation):
//! `<profile>/egress/mappings.json` (0600) maps each secret of
//! `security.egress.inject` to the placeholder container and remote
//! sandboxes hold. Placeholders live as long as their mapping: setup
//! keeps existing ones and mints only for new mappings; `egress rotate`
//! backs the file up (0600) and mints new ones for every mapping.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_net::egress::inject::placeholder;

/// The mapping file's name under `<profile>/egress/`.
pub const FILE: &str = "mappings.json";

/// Secret name to placeholder.
pub type Placeholders = BTreeMap<String, String>;

/// The mapping file of `scope`.
#[must_use]
pub fn path(scope: &ProfileScope) -> PathBuf {
    super::injection::ca_dir(scope).join(FILE)
}

/// The placeholders on disk (empty when there is no file).
#[must_use]
pub fn read(file: &Path) -> Placeholders {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write(file: &Path, placeholders: &Placeholders) -> Result<(), Error> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(&codes::HOME_IO, dir, &e))?;
    }
    let text = serde_json::to_vec_pretty(placeholders).expect("placeholders serialize");
    pmt_core::private_fs::write_atomic(file, &text)
        .map_err(|e| Error::io(&codes::HOME_IO, file, &e))
}

/// The placeholders of the current `security.egress.inject` mappings:
/// existing ones kept, new mappings minted, dropped ones removed.
///
/// # Errors
///
/// `home.io` when the file cannot be written.
pub fn ensure(scope: &ProfileScope) -> Result<Placeholders, Error> {
    let file = path(scope);
    let old = read(&file);
    let mapped = &scope.config().security.egress.inject;
    let new: Placeholders = mapped
        .keys()
        .map(|name| {
            let token = old.get(name).cloned().unwrap_or_else(placeholder);
            (name.clone(), token)
        })
        .collect();
    if new != old {
        write(&file, &new)?;
    }
    Ok(new)
}

/// New placeholders for every mapping, after backing up the old file as
/// `mappings.json.bak-<unix seconds>` (0600); returns the backup's path.
///
/// # Errors
///
/// `home.io`.
pub fn rotate(scope: &ProfileScope) -> Result<Option<PathBuf>, Error> {
    let file = path(scope);
    let backup = match std::fs::read(&file) {
        Ok(old) => {
            let stamp = jiff::Timestamp::now().as_second();
            let backup = file.with_file_name(format!("{FILE}.bak-{stamp}"));
            pmt_core::private_fs::write_atomic(&backup, &old)
                .map_err(|e| Error::io(&codes::HOME_IO, &backup, &e))?;
            Some(backup)
        }
        Err(_) => None,
    };
    let fresh: Placeholders = scope
        .config()
        .security
        .egress
        .inject
        .keys()
        .map(|name| (name.clone(), placeholder()))
        .collect();
    write(&file, &fresh)?;
    Ok(backup)
}

/// `pmt-egress-1a2b…9z` shown as its first and last characters only.
#[must_use]
pub fn redacted(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() <= 16 {
        return "…".to_owned();
    }
    let head: String = chars[..12].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

//! The official optional catalog embedded in the binary (trust
//! `builtin`): installable as `official/<category>/<name>`, browsable
//! offline.

use std::collections::BTreeMap;

use pmt_core::error::{Error, codes};
use pmt_skills::frontmatter;
use pmt_skills::layout::SKILL_FILE;
use pmt_skills::lockfile::Trust;

use crate::bundle::Bundle;
use crate::hub::Hub;
use crate::ident::Identifier;
use crate::sources::Listing;

/// The `resolved_ref` of an official install: the embedding release.
#[must_use]
pub fn resolved_ref() -> String {
    format!("embedded@{}", env!("CARGO_PKG_VERSION"))
}

/// Fetches `official/<category>/<name>`.
///
/// # Errors
///
/// `skill.not_found`.
pub fn fetch(hub: &Hub, category: &str, name: &str) -> Result<Bundle, Error> {
    let prefix = format!("{category}/{name}");
    let files: BTreeMap<String, Vec<u8>> = hub
        .official
        .files_in(&prefix)
        .into_iter()
        .filter_map(|rel| {
            let bytes = hub.official.read(&prefix, &rel)?;
            Some((rel, bytes.to_vec()))
        })
        .collect();
    if !files.contains_key(SKILL_FILE) {
        return Err(Error::new(
            &codes::SKILL_NOT_FOUND,
            format!("no official skill 'official/{prefix}'"),
        ));
    }
    let id = Identifier::Official {
        category: category.to_owned(),
        name: name.to_owned(),
    };
    let mut bundle = Bundle::new(id, files, resolved_ref(), Trust::Builtin)?;
    bundle.category = Some(category.to_owned());
    if bundle.name.is_empty() {
        name.clone_into(&mut bundle.name);
    }
    Ok(bundle)
}

/// Every official skill, by category then name.
#[must_use]
pub fn listings(hub: &Hub) -> Vec<Listing> {
    pmt_skills::official::skills(&hub.official)
        .into_iter()
        .filter_map(|(name, prefix)| {
            let text = hub.official.read(&prefix, SKILL_FILE)?;
            let front = frontmatter::mapping(&String::from_utf8_lossy(text));
            let description = front
                .as_ref()
                .and_then(|f| f.get("description"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Some(Listing {
                identifier: format!("official/{prefix}"),
                name,
                description,
                source: "official".to_owned(),
                trust: Trust::Builtin,
                installs: None,
            })
        })
        .collect()
}

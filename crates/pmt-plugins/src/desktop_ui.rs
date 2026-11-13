//! Desktop panels (spec: desktop-app, Plugin UI contributions are
//! isolated): the `[ui]` panels of the profile's installed, enabled
//! plugins. A panel loads only from a plugin whose package signature was
//! verified at install, or that was installed with `--allow-unsigned`
//! (its install record says which); embedded plugins ship with the binary.

use std::path::PathBuf;

use serde::Serialize;

use crate::discovery::{Discovered, Source};
use crate::load::{State, Status};
use crate::record::InstallRecord;

/// One panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Panel {
    /// The plugin id (its frame's origin is `plugin-<id>.localhost`).
    pub id: String,
    /// Its label.
    pub label: String,
    /// The HTML file the frame loads.
    pub entry: PathBuf,
    /// The SDK capabilities granted to it.
    pub sdk: Vec<String>,
}

/// A panel that is not loaded, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    /// The plugin id.
    pub id: String,
    /// Why.
    pub reason: String,
}

/// What the desktop loads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Panels {
    /// By plugin id.
    pub panels: Vec<Panel>,
    /// Declared but not loaded.
    pub skipped: Vec<Skipped>,
}

fn signature_allows(found: &Discovered) -> Result<(), String> {
    if found.source == Source::Embedded {
        return Ok(());
    }
    let record = found
        .dir
        .as_deref()
        .and_then(|dir| InstallRecord::read(dir).ok().flatten());
    match record {
        Some(r) if r.signer.is_some() || r.flags.allow_unsigned => Ok(()),
        Some(_) => Err(
            "its package signature was not verified and it was not installed with --allow-unsigned"
                .to_owned(),
        ),
        None => Err("it has no install record".to_owned()),
    }
}

/// The panels of the plugins `surveyed` (the profile's discovery).
#[must_use]
pub fn panels(surveyed: &[(Discovered, Status)]) -> Panels {
    let mut out = Panels::default();
    for (found, status) in surveyed {
        let Some(declared) = found.manifest.as_ref().ok().and_then(|m| m.ui.as_ref()) else {
            continue;
        };
        let Some(dir) = found.dir.as_deref() else {
            continue;
        };
        if status.state != State::Loaded {
            out.skipped.push(Skipped {
                id: status.id.clone(),
                reason: status
                    .problem
                    .clone()
                    .unwrap_or_else(|| "it is not enabled".to_owned()),
            });
            continue;
        }
        if let Err(reason) = signature_allows(found) {
            out.skipped.push(Skipped {
                id: status.id.clone(),
                reason,
            });
            continue;
        }
        out.panels.push(Panel {
            id: status.id.clone(),
            label: declared.label.clone(),
            entry: dir.join(&declared.entry),
            sdk: declared.sdk.clone(),
        });
    }
    out.panels.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

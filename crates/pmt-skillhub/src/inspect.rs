//! `prometheus skills inspect` (spec: skills, Hub discovery, inspection,
//! updates and audit): fetch into memory, scan there, and show the
//! preview; nothing is written anywhere.

use pmt_core::error::Error;
use pmt_skills::scan::Report;
use serde::Serialize;

use crate::bundle::Upstream;
use crate::hub::Hub;
use crate::ident::Identifier;
use crate::sign::{self, SIGNATURE_FILE};
use crate::sources;

/// What `inspect` shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Inspection {
    /// The canonical identifier.
    pub identifier: String,
    /// The skill name (empty when the source gives none).
    pub name: String,
    /// The description.
    pub description: String,
    /// `builtin`, `trusted` or `community`.
    pub trust: String,
    /// The ref the source resolved.
    pub resolved_ref: String,
    /// Every file.
    pub files: Vec<String>,
    /// The `SKILL.md` text.
    pub skill_md: String,
    /// The scan.
    pub scan: Report,
    /// `verified:<identity>`, `unsigned` or `invalid`.
    pub signature: String,
    /// Upstream metadata.
    pub upstream: Upstream,
}

/// Inspects `id` without installing it.
///
/// # Errors
///
/// The fetch's errors; a scanner failure is reported as a `dangerous`
/// verdict.
pub fn inspect(hub: &Hub, id: &Identifier) -> Result<Inspection, Error> {
    let bundle = sources::fetch(hub, id, None)?;
    let scan = hub
        .scanner
        .scan(&bundle.scan_input())
        .unwrap_or_else(|e| Report {
            verdict: pmt_skills::scan::Verdict::Dangerous,
            findings: vec![pmt_skills::scan::Finding {
                pattern: format!("scan_failed: {e}"),
                severity: pmt_skills::scan::Verdict::Dangerous,
                file: String::new(),
                line: 0,
            }],
        });
    let signature = sign::verify(
        bundle.files.get(SIGNATURE_FILE).map(Vec::as_slice),
        &bundle.signed_digest(),
        &hub.roots,
    );
    Ok(Inspection {
        identifier: bundle.identifier.canonical(),
        name: bundle.name.clone(),
        description: bundle.description(),
        trust: sources::trust_text(bundle.trust).to_owned(),
        resolved_ref: bundle.resolved_ref.clone(),
        files: bundle.files.keys().cloned().collect(),
        skill_md: bundle.skill_md(),
        scan,
        signature: signature.record(),
        upstream: bundle.upstream,
    })
}

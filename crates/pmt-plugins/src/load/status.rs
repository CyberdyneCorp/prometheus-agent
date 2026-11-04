//! The state of a discovered plugin before anything is instantiated
//! (spec: plugin-system, Opt-in enablement; Capability grants are
//! user-approved and digest-bound; Catalog kill list; plugins CLI; Safe
//! mode; security-boundaries, Signed plugins).
//!
//! In order: a manifest error; a directory in the profile without an
//! install record (`unadopted`); one whose content no longer matches its
//! record (`plugin '<id>' was modified after install`); a plugin on the
//! catalog's kill list; then enablement, safe mode, kind and approval.

use pmt_config::grants::Grants;

use super::{LoadInputs, SAFE_MODE_NOTE, State, Status, identity, requested};
use crate::discovery::{Discovered, Enablement, Source};
use crate::grants::Approval;
use crate::manifest::{Kind, Manifest};
use crate::record::{InstallRecord, Origin};

/// The note of an unadopted plugin.
pub const UNADOPTED_NOTE: &str =
    "placed by hand without an install record: run `prometheus plugins adopt <dir>`";

/// The status before instantiation: `Loaded` means "to instantiate".
pub(super) fn status_of(plugin: &Discovered, inputs: &LoadInputs<'_>, grants: &Grants) -> Status {
    let manifest = plugin.manifest.as_ref().ok();
    let mut status = Status {
        id: plugin.id.clone(),
        version: manifest.map(|m| m.version.to_string()),
        kind: manifest.map(|m| m.kind),
        source: plugin.source.clone(),
        digest: plugin.digest.clone(),
        state: State::Disabled,
        problem: None,
        shadowed: plugin.shadowed.clone(),
    };
    if let Some((state, problem)) = provenance(plugin, inputs) {
        status.state = state;
        status.problem = Some(problem);
        return status;
    }
    let enablement = Enablement::of(plugin, inputs.config);
    let safe_mode = inputs.config.safe_mode && plugin.embedded.is_none();
    let (state, problem) = match (&plugin.manifest, enablement) {
        (Err(err), _) => (State::Error, Some(err.message().to_owned())),
        (Ok(_), Enablement::Enabled) if safe_mode => {
            (State::Disabled, Some(SAFE_MODE_NOTE.to_owned()))
        }
        (Ok(_), Enablement::Disabled) => (State::Disabled, None),
        (Ok(_), Enablement::Conflict) => {
            (State::Disabled, Some(Enablement::CONFLICT_NOTE.to_owned()))
        }
        (Ok(manifest), Enablement::Enabled) => runnable(plugin, manifest, inputs, grants),
    };
    status.state = state;
    status.problem = problem;
    status
}

fn runnable(
    plugin: &Discovered,
    manifest: &Manifest,
    inputs: &LoadInputs<'_>,
    grants: &Grants,
) -> (State, Option<String>) {
    if manifest.kind == Kind::Mcp {
        return (
            State::Error,
            Some("mcp plugins are hosted by the MCP client, which is not available yet".to_owned()),
        );
    }
    if manifest.kind == Kind::Python
        && let Some(problem) = crate::python::PythonLaunch::of(manifest).compat_problem()
    {
        return (State::Error, Some(problem));
    }
    let Some(identity) = identity(plugin) else {
        return (State::Error, Some("plugin files unreadable".to_owned()));
    };
    let requested = match requested(manifest, inputs) {
        Ok(r) => r,
        Err(err) => return (State::Error, Some(err.message().to_owned())),
    };
    match Approval::of(grants, &identity, &requested).problem(&manifest.id) {
        Some(problem) => (State::Error, Some(problem)),
        None => (State::Loaded, None),
    }
}

/// What the install record and the kill list say, when that decides.
fn provenance(plugin: &Discovered, inputs: &LoadInputs<'_>) -> Option<(State, String)> {
    if plugin.manifest.is_err() || plugin.embedded.is_some() {
        return None;
    }
    let record = match (&plugin.source, &plugin.dir) {
        (Source::User, Some(dir)) => match InstallRecord::read(dir) {
            Ok(Some(record)) => Some(record),
            Ok(None) => return Some((State::Unadopted, UNADOPTED_NOTE.to_owned())),
            Err(err) => return Some((State::Error, err.message().to_owned())),
        },
        _ => None,
    };
    if let Some(record) = &record
        && plugin.digest.as_deref() != Some(record.digest.as_str())
    {
        return Some((
            State::Error,
            format!("plugin '{}' was modified after install", plugin.id),
        ));
    }
    let repo = record
        .as_ref()
        .filter(|r| matches!(r.source, Origin::Git | Origin::Catalog))
        .map(|r| r.location.as_str());
    let allowed = record.as_ref().is_some_and(|r| r.flags.allow_removed);
    let catalog = crate::catalog::Catalog {
        removed: inputs.blocked.to_vec(),
        ..crate::catalog::Catalog::default()
    };
    match catalog.blocked(&plugin.id, repo) {
        Some(removed) if !allowed => Some((
            State::Error,
            removed.refusal(&plugin.id).message().to_owned(),
        )),
        _ => None,
    }
}

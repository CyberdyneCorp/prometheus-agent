//! `prometheus auth import codex-cli [--path P] [--label L]`: copies a
//! Codex CLI sign-in into the secrets store (the file is only read).

use std::path::Path;

use pmt_core::error::{Error, codes};
use pmt_providers::auth::DEFAULT_LABEL;
use pmt_providers::auth::codex;
use pmt_providers::auth::credentials::check_label;
use serde::Serialize;

use super::Profile;
use crate::context::Context;
use crate::output::json;

#[derive(Serialize)]
struct Imported<'a> {
    profile: &'a str,
    provider: &'a str,
    label: &'a str,
    kind: &'static str,
    source: &'a str,
    warning: &'static str,
}

pub(super) fn codex_cli(
    ctx: &Context<'_>,
    path: Option<&Path>,
    label: Option<&str>,
) -> Result<u8, Error> {
    let label = label.unwrap_or(DEFAULT_LABEL);
    check_label(label)?;
    let path = if let Some(path) = path {
        path.to_owned()
    } else {
        let home = ctx.launch.user_home().ok_or_else(|| {
            Error::new(
                &codes::AUTH_IMPORT_INCOMPLETE,
                "nothing was imported: the user's home directory is unknown",
            )
            .with_hint("pass the file: prometheus auth import codex-cli --path <auth.json>")
        })?;
        codex::default_path(home)
    };
    let (tokens, obtained) = codex::read(&path)?;
    let profile = Profile::open(ctx)?;
    let provider = profile.registry.lookup(codex::PROVIDER)?;
    let sub = profile
        .credentials
        .save(provider.id(), label, &tokens, &obtained)?;
    if ctx.json {
        return json(&Imported {
            profile: profile.scope.name().as_str(),
            provider: provider.id(),
            label,
            kind: "subscription",
            source: &sub.source,
            warning: codex::SHARED_FAMILY_WARNING,
        });
    }
    eprintln!(
        "imported the Codex CLI sign-in as {} '{label}'",
        provider.id()
    );
    eprintln!("warning: {}", codex::SHARED_FAMILY_WARNING);
    Ok(0)
}

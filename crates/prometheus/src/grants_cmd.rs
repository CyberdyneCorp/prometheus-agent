//! `prometheus grants list|revoke` (spec: security-boundaries, Grants are
//! stored, reviewable and revocable). Both work on the selected profile's
//! `grants.toml`; a revocation reaches running sessions at their next tool
//! call because sessions re-read the file when it changes.

use std::fmt::Write as _;

use pmt_config::grants::{Grant, GrantsFile, Subject};
use pmt_core::error::Error;
use pmt_core::id::GrantId;
use serde::Serialize;

use crate::cli::GrantsCommand;
use crate::context::Context;
use crate::output::{document, json, print_raw};

/// Runs one `grants` subcommand and returns its exit code.
///
/// # Errors
///
/// The command's failure; the caller prints it and exits with its code.
pub fn run(command: &GrantsCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        GrantsCommand::List { subject } => list(ctx, subject.as_deref()),
        GrantsCommand::Revoke { id } => revoke(ctx, id),
    }
}

fn list(ctx: &Context<'_>, subject: Option<&str>) -> Result<u8, Error> {
    #[derive(Serialize)]
    struct Listing<'a> {
        profile: &'a str,
        grants: Vec<Grant>,
        approvals: Vec<String>,
    }
    let subject: Option<Subject> = subject.map(str::parse).transpose()?;
    let profile = ctx.profile_dir()?;
    let mut contents = GrantsFile::of(&profile).load()?;
    if let Some(subject) = &subject {
        contents.grants.retain(|g| &g.subject == subject);
        // Command approvals belong to the profile, not to a subject.
        contents.allow.clear();
    }
    if ctx.json {
        return document(
            "grants list",
            &Listing {
                profile: profile.name().as_str(),
                grants: contents.grants,
                approvals: contents.allow,
            },
        );
    }
    let mut text = String::new();
    for g in &contents.grants {
        let _ = writeln!(
            text,
            "{}  {}  {}  {}  {}",
            g.id, g.subject, g.capability, g.surface, g.granted_at
        );
    }
    for pattern in &contents.allow {
        let _ = writeln!(text, "approval  {pattern}");
    }
    print_raw(&text)
}

fn revoke(ctx: &Context<'_>, id: &str) -> Result<u8, Error> {
    let id = GrantId::parse(id)?;
    let profile = ctx.profile_dir()?;
    let revoked = GrantsFile::of(&profile).revoke(id)?;
    if ctx.json {
        return json(&revoked);
    }
    eprintln!(
        "revoked {} ({} for {}) in profile '{}'",
        revoked.id,
        revoked.capability,
        revoked.subject,
        profile.name()
    );
    Ok(0)
}

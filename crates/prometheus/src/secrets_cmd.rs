//! `prometheus secrets` (spec: configuration-and-profiles, Secrets store;
//! security-boundaries, Secrets storage). Values enter only through a
//! no-echo prompt or stdin and leave only through `get --reveal` on a
//! terminal; listings, JSON documents and messages carry names only.

use pmt_config::ProfileScope;
use pmt_config::schema::SecretsBackend;
use pmt_config::secrets::{Listed, SecretStore, Source};
use pmt_core::error::{Error, codes};
use serde::Serialize;

use crate::cli::{SecretsBackendArg, SecretsCommand};
use crate::context::Context;
use crate::output::{document, json, print, print_raw};
use crate::prompt;

/// Runs one `secrets` subcommand and returns its exit code.
///
/// # Errors
///
/// The command's failure; the caller prints it and exits with its code.
pub fn run(command: &SecretsCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        SecretsCommand::Set { name, value } => set(ctx, name, value),
        SecretsCommand::List => list(ctx),
        SecretsCommand::Get { name, reveal } => get(ctx, name, *reveal),
        SecretsCommand::Unset { name } => unset(ctx, name),
        SecretsCommand::Migrate { to } => migrate(ctx, backend(*to)),
    }
}

fn backend(arg: SecretsBackendArg) -> SecretsBackend {
    match arg {
        SecretsBackendArg::File => SecretsBackend::File,
        SecretsBackendArg::Keychain => SecretsBackend::Keychain,
    }
}

fn backend_name(backend: SecretsBackend) -> &'static str {
    match backend {
        SecretsBackend::File => "file",
        SecretsBackend::Keychain => "keychain",
    }
}

#[derive(Serialize)]
struct Changed<'a> {
    profile: &'a str,
    name: &'a str,
    backend: &'static str,
    #[serde(rename = "changed")]
    done: bool,
}

fn set(ctx: &Context<'_>, name: &str, argv_value: &[String]) -> Result<u8, Error> {
    if !argv_value.is_empty() {
        return Err(Error::new(
            &codes::SECRET_VALUE_ON_ARGV,
            "secret values are never accepted as arguments",
        )
        .with_hint(format!(
            "type it at the hidden prompt of `prometheus secrets set {name}`, or pipe it on stdin"
        )));
    }
    pmt_config::secrets::check_name(name)?;
    let scope = ctx.scope()?;
    let value = prompt::read_secret(name)?;
    let store = scope.secrets().store();
    store.set(name, &value)?;
    report(ctx, &scope, name, true, "stored")
}

fn unset(ctx: &Context<'_>, name: &str) -> Result<u8, Error> {
    pmt_config::secrets::check_name(name)?;
    let scope = ctx.scope()?;
    let removed = scope.secrets().store().remove(name)?;
    report(
        ctx,
        &scope,
        name,
        removed,
        if removed { "removed" } else { "was not stored" },
    )
}

fn report(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    name: &str,
    changed: bool,
    verb: &str,
) -> Result<u8, Error> {
    let backend = backend_name(scope.secrets().backend());
    if ctx.json {
        let profile = scope.name().as_str();
        return json(&Changed {
            profile,
            name,
            backend,
            done: changed,
        });
    }
    eprintln!(
        "{name} {verb} ({backend} store of profile '{}')",
        scope.name()
    );
    Ok(0)
}

fn list(ctx: &Context<'_>) -> Result<u8, Error> {
    #[derive(Serialize)]
    struct Listing<'a> {
        profile: &'a str,
        backend: &'static str,
        secrets: Vec<Listed>,
    }
    let scope = ctx.scope()?;
    let secrets = scope.secrets().list()?;
    if ctx.json {
        return document(
            "secrets list",
            &Listing {
                profile: scope.name().as_str(),
                backend: backend_name(scope.secrets().backend()),
                secrets,
            },
        );
    }
    let text: String = secrets.iter().map(|row| describe(row) + "\n").collect();
    print_raw(&text)
}

/// `NAME  source[ (VARIABLE)][  shadowed]`.
fn describe(row: &Listed) -> String {
    let source = match row.source {
        Source::File => "file",
        Source::Keychain => "keychain",
        Source::Env => "env",
    };
    let variable = row
        .variable
        .as_deref()
        .filter(|v| *v != row.name)
        .map(|v| format!(" ({v})"))
        .unwrap_or_default();
    let shadowed = if row.shadowed { "  shadowed" } else { "" };
    format!("{}  {source}{variable}{shadowed}", row.name)
}

fn get(ctx: &Context<'_>, name: &str, reveal: bool) -> Result<u8, Error> {
    pmt_config::secrets::check_name(name)?;
    if reveal && !prompt::stdout_is_tty() {
        return Err(Error::new(
            &codes::SECRET_REVEAL_REFUSED,
            "refusing to reveal a secret: stdout is not a terminal",
        ));
    }
    let scope = ctx.scope()?;
    let value = scope.secrets().resolve(name)?;
    if reveal {
        return print(&value.expose());
    }
    let row = scope
        .secrets()
        .list()?
        .into_iter()
        .find(|row| row.name == name && !row.shadowed)
        .expect("a resolved secret is listed");
    if ctx.json {
        return json(&row);
    }
    print(&describe(&row))
}

fn migrate(ctx: &Context<'_>, to: SecretsBackend) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let from = scope.secrets().store();
    if from.kind() == to {
        eprintln!(
            "profile '{}' already uses the {} backend",
            scope.name(),
            backend_name(to)
        );
        return Ok(0);
    }
    let target = SecretStore::open(scope.profile(), to);
    let moved = pmt_config::secrets::migrate(scope.profile(), from, &target)?;
    eprintln!(
        "moved {moved} secret(s) of profile '{}' to the {} backend",
        scope.name(),
        backend_name(to)
    );
    Ok(0)
}

//! `prometheus secrets onepassword` over the `op` CLI.

use std::fmt::Write as _;
use std::path::Path;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;

use super::{
    OP, OnePasswordCommand, configure, preview_line, replace_token, shadowing, stored_token,
};
use crate::context::Context;
use crate::output::print_raw;

pub(super) fn run(command: &OnePasswordCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        OnePasswordCommand::Setup { vault, yes } => setup(ctx, vault.as_deref(), *yes),
        OnePasswordCommand::Status => status(ctx),
        OnePasswordCommand::Map { name, reference } => map(ctx, name, reference),
        OnePasswordCommand::Unmap { name } => {
            let scope = ctx.scope()?;
            let removed = scope
                .profile()
                .config_file()
                .unset(&format!("secrets.onepassword.map.{name}"))?;
            eprintln!(
                "{name} {}",
                if removed {
                    "unmapped"
                } else {
                    "was not mapped"
                }
            );
            Ok(0)
        }
        OnePasswordCommand::Sync { apply } => sync(ctx, *apply),
        OnePasswordCommand::Disable => {
            configure(&ctx.scope()?, &[("secrets.onepassword.enabled", "false")])?;
            eprintln!("the 1Password source is off");
            Ok(0)
        }
    }
}

fn cli(ctx: &Context<'_>) -> Result<std::path::PathBuf, Error> {
    let home = ctx.home()?;
    OP.managed(home.root()).ok_or_else(|| {
        super::refused(
            "op",
            "not installed; run prometheus secrets onepassword setup",
        )
    })
}

/// The vault names the token sees (`op vault list`): fails for a token
/// 1Password refuses.
fn vaults(program: &Path, token: &SecretString) -> Result<Vec<String>, Error> {
    let out = OP.run(program, &["vault", "list", "--format", "json"], token)?;
    let listed: Vec<serde_json::Value> = serde_json::from_slice(&out)
        .map_err(|_| super::refused("op", "vault list did not print a JSON list"))?;
    Ok(listed
        .iter()
        .filter_map(|v| v.get("name").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect())
}

fn setup(ctx: &Context<'_>, vault: Option<&str>, yes: bool) -> Result<u8, Error> {
    let program = match OP.ensure(ctx, yes)? {
        Ok(program) => program,
        Err(code) => return Ok(code),
    };
    let scope = ctx.scope()?;
    let config = scope.config().secrets.onepassword.clone();
    let mut seen = Vec::new();
    replace_token(&scope, &config.access_token_secret.0, |token| {
        seen = vaults(&program, token)?;
        Ok(())
    })?;
    let chosen = match vault.map(str::to_owned).or(config.vault) {
        Some(vault) if seen.contains(&vault) => Some(vault),
        Some(vault) => {
            return Err(Error::new(
                &codes::SECRET_SOURCE_FAILED,
                format!(
                    "the token sees no vault named '{vault}' ({})",
                    seen.join(", ")
                ),
            ));
        }
        None if seen.len() == 1 => seen.pop(),
        None => None,
    };
    let quoted = chosen.as_ref().map(|v| format!("{v:?}"));
    let mut pairs = vec![("secrets.onepassword.enabled", "true")];
    if let Some(quoted) = &quoted {
        pairs.push(("secrets.onepassword.vault", quoted));
    } else {
        eprintln!("note: no vault selected; pass --vault <name> for sync");
    }
    configure(&scope, &pairs)?;
    eprintln!("the 1Password source is on for profile '{}'", scope.name());
    Ok(0)
}

fn status(ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let config = scope.config().secrets.onepassword.clone();
    let home = ctx.home()?;
    let program = OP.managed(home.root());
    let mut out = format!(
        "enabled: {}\nop: {}\nvault: {}\n",
        if config.enabled { "yes" } else { "no" },
        program
            .as_deref()
            .and_then(super::Cli::version)
            .unwrap_or_else(|| "not installed".to_owned()),
        config.vault.as_deref().unwrap_or("(none)")
    );
    let valid = match (
        &program,
        stored_token(&scope, &config.access_token_secret.0),
    ) {
        (Some(program), Ok(token)) => vaults(program, &token).map(|_| ()),
        (_, Err(err)) => Err(err),
        (None, _) => Err(super::refused("op", "not installed")),
    };
    match valid {
        Ok(()) => out.push_str("token: valid\n"),
        Err(err) => {
            let _ = writeln!(out, "token: not usable ({})", err.message());
        }
    }
    let names: Vec<&str> = config.map.keys().map(String::as_str).collect();
    let _ = writeln!(out, "mapped: {}", names.join(", "));
    print_raw(&out)
}

fn map(ctx: &Context<'_>, name: &str, reference: &str) -> Result<u8, Error> {
    pmt_config::secrets::check_name(name)?;
    if !reference.starts_with("op://") {
        return Err(Error::new(
            &codes::CLI_USAGE,
            "a 1Password reference looks like op://vault/item/field",
        ));
    }
    let scope = ctx.scope()?;
    set_map(&scope, &[(name.to_owned(), reference.to_owned())])?;
    eprintln!("{name} maps to {reference}");
    Ok(0)
}

/// Writes `entries` into `secrets.onepassword.map`.
fn set_map(scope: &ProfileScope, entries: &[(String, String)]) -> Result<(), Error> {
    let values: Vec<(String, toml_edit::Value)> = entries
        .iter()
        .map(|(name, reference)| {
            (
                format!("secrets.onepassword.map.{name}"),
                toml_edit::Value::from(reference.as_str()),
            )
        })
        .collect();
    scope.profile().config_file().set_values(&values)
}

/// The vault's items titled like secret names, as references to their
/// `credential` field.
fn proposals(
    program: &Path,
    vault: &str,
    token: &SecretString,
) -> Result<Vec<(String, String)>, Error> {
    let out = OP.run(
        program,
        &["item", "list", "--vault", vault, "--format", "json"],
        token,
    )?;
    let listed: Vec<serde_json::Value> = serde_json::from_slice(&out)
        .map_err(|_| super::refused("op", "item list did not print a JSON list"))?;
    let mut found: Vec<(String, String)> = listed
        .iter()
        .filter_map(|item| item.get("title").and_then(serde_json::Value::as_str))
        .filter(|title| pmt_config::schema::is_secret_name(title))
        .map(|title| (title.to_owned(), format!("op://{vault}/{title}/credential")))
        .collect();
    found.sort();
    found.dedup_by(|a, b| a.0 == b.0);
    Ok(found)
}

fn sync(ctx: &Context<'_>, apply: bool) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let config = scope.config().secrets.onepassword.clone();
    let vault = config.vault.clone().ok_or_else(|| {
        Error::new(
            &codes::SECRET_SOURCE_FAILED,
            "no vault selected; run prometheus secrets onepassword setup --vault <name>",
        )
    })?;
    let program = cli(ctx)?;
    let token = stored_token(&scope, &config.access_token_secret.0)?;
    let shadowed = shadowing(&scope)?;
    let new: Vec<(String, String)> = proposals(&program, &vault, &token)?
        .into_iter()
        .filter(|(name, _)| !config.map.contains_key(name))
        .collect();
    let text: String = new
        .iter()
        .map(|(name, reference)| {
            preview_line(name, shadowed.contains(name), &format!("  {reference}"))
        })
        .collect();
    print_raw(&text)?;
    if apply && !new.is_empty() {
        set_map(&scope, &new)?;
        eprintln!("mapped {} name(s)", new.len());
    } else if !apply {
        eprintln!("nothing written; rerun with --apply to map these names");
    }
    Ok(0)
}

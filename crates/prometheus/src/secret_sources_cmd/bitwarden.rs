//! `prometheus secrets bitwarden` over the `bws` CLI.

use std::fmt::Write as _;
use std::path::Path;

use pmt_config::ProfileScope;
use pmt_config::schema::BitwardenSourceConfig;
use pmt_core::error::Error;
use pmt_core::secret::SecretString;

use super::{
    BWS, BitwardenCommand, configure, preview_line, replace_token, shadowing, stored_token,
    toml_list,
};
use crate::context::Context;
use crate::output::print_raw;

pub(super) fn run(command: &BitwardenCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        BitwardenCommand::Setup {
            project_id,
            server_url,
            yes,
        } => setup(ctx, project_id.as_deref(), server_url.as_deref(), *yes),
        BitwardenCommand::Status => status(ctx),
        BitwardenCommand::Token => {
            let scope = ctx.scope()?;
            let program = cli(ctx)?;
            let config = scope.config().secrets.bitwarden.clone();
            replace_token(&scope, &config.access_token_secret.0, |token| {
                check(&program, &config, token)
            })?;
            Ok(0)
        }
        BitwardenCommand::Sync { apply } => sync(ctx, *apply),
        BitwardenCommand::Install { yes } => Ok(BWS.install(ctx, *yes)?.unwrap_or(0)),
        BitwardenCommand::Disable => {
            configure(&ctx.scope()?, &[("secrets.bitwarden.enabled", "false")])?;
            eprintln!("the Bitwarden source is off");
            Ok(0)
        }
    }
}

/// The managed `bws`, which every subcommand but setup and install needs.
fn cli(ctx: &Context<'_>) -> Result<std::path::PathBuf, Error> {
    let home = ctx.home()?;
    BWS.managed(home.root()).ok_or_else(|| {
        super::refused(
            "bws",
            "not installed; run prometheus secrets bitwarden setup (or install)",
        )
    })
}

/// `bws project list`: succeeds only for a token the server accepts.
fn check(
    program: &Path,
    config: &BitwardenSourceConfig,
    token: &SecretString,
) -> Result<(), Error> {
    let mut args = vec!["project", "list", "--output", "json"];
    if let Some(url) = &config.server_url {
        args.extend(["--server-url", url]);
    }
    BWS.run(program, &args, token).map(|_| ())
}

/// The project's secret keys (`bws secret list`), names only.
fn keys(
    program: &Path,
    config: &BitwardenSourceConfig,
    token: &SecretString,
) -> Result<Vec<String>, Error> {
    let mut args = vec!["secret", "list"];
    if let Some(project) = &config.project_id {
        args.push(project);
    }
    args.extend(["--output", "json"]);
    if let Some(url) = &config.server_url {
        args.extend(["--server-url", url]);
    }
    let out = BWS.run(program, &args, token)?;
    let listed: Vec<serde_json::Value> = serde_json::from_slice(&out)
        .map_err(|_| super::refused("bws", "secret list did not print a JSON list"))?;
    let mut names: Vec<String> = listed
        .iter()
        .filter_map(|e| e.get("key").and_then(serde_json::Value::as_str))
        .filter(|k| pmt_config::schema::is_secret_name(k))
        .map(str::to_owned)
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

fn setup(
    ctx: &Context<'_>,
    project_id: Option<&str>,
    server_url: Option<&str>,
    yes: bool,
) -> Result<u8, Error> {
    let program = match BWS.ensure(ctx, yes)? {
        Ok(program) => program,
        Err(code) => return Ok(code),
    };
    let scope = ctx.scope()?;
    let mut config = scope.config().secrets.bitwarden.clone();
    config.server_url = server_url.map(str::to_owned).or(config.server_url);
    config.project_id = project_id.map(str::to_owned).or(config.project_id);
    replace_token(&scope, &config.access_token_secret.0, |token| {
        check(&program, &config, token)
    })?;
    let mut pairs = vec![("secrets.bitwarden.enabled", "true")];
    let (project, url);
    if let Some(id) = &config.project_id {
        project = format!("{id:?}");
        pairs.push(("secrets.bitwarden.project_id", &project));
    }
    if let Some(server) = &config.server_url {
        url = format!("{server:?}");
        pairs.push(("secrets.bitwarden.server_url", &url));
    }
    configure(&scope, &pairs)?;
    eprintln!("the Bitwarden source is on for profile '{}'", scope.name());
    Ok(0)
}

fn status(ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let config = scope.config().secrets.bitwarden.clone();
    let home = ctx.home()?;
    let program = BWS.managed(home.root());
    let mut out = format!(
        "enabled: {}\nbws: {}\n",
        if config.enabled { "yes" } else { "no" },
        program
            .as_deref()
            .and_then(super::Cli::version)
            .unwrap_or_else(|| "not installed".to_owned())
    );
    let token = stored_token(&scope, &config.access_token_secret.0);
    let names = match (&program, &token) {
        (Some(program), Ok(token)) => keys(program, &config, token),
        (_, Err(err)) => Err(Error::new(err.code(), err.message())),
        (None, _) => Err(super::refused("bws", "not installed")),
    };
    match &names {
        Ok(names) => {
            out.push_str("token: valid\n");
            let accepted = |n: &String| config.names.is_empty() || config.names.contains(n);
            let supplied: Vec<&String> = names.iter().filter(|n| accepted(n)).collect();
            let listed: Vec<&str> = supplied.iter().map(|n| n.as_str()).collect();
            let _ = writeln!(out, "names: {}", listed.join(", "));
        }
        Err(err) => {
            let _ = writeln!(out, "token: not usable ({})", err.message());
        }
    }
    print_raw(&out)
}

fn sync(ctx: &Context<'_>, apply: bool) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let config = scope.config().secrets.bitwarden.clone();
    let program = cli(ctx)?;
    let token = stored_token(&scope, &config.access_token_secret.0)?;
    let names = keys(&program, &config, &token)?;
    let shadowed = shadowing(&scope)?;
    let text: String = names
        .iter()
        .map(|n| preview_line(n, shadowed.contains(n), ""))
        .collect();
    print_raw(&text)?;
    if apply {
        record(&scope, &names)?;
        eprintln!("accepted {} name(s) from Bitwarden", names.len());
    } else {
        eprintln!("nothing written; rerun with --apply to accept these names");
    }
    Ok(0)
}

fn record(scope: &ProfileScope, names: &[String]) -> Result<(), Error> {
    configure(scope, &[("secrets.bitwarden.names", &toml_list(names))])
}

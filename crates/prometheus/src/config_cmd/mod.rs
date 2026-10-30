//! `prometheus config` (spec: configuration-and-profiles, Config CLI): every
//! read goes through a [`ProfileScope`], every write through the one
//! format-preserving writer, and the schema, docs and `explain` come from
//! the typed schema.

mod edit;
mod explain;

use pmt_config::{catalog, reference};
use pmt_core::error::{Error, codes};
use serde::Serialize;
use serde_json::Value;

use crate::cli::ConfigCommand;
pub use crate::context::Context;
use crate::output::{document, print, print_raw};

/// Runs one `config` subcommand and returns its exit code.
///
/// # Errors
///
/// The command's failure; the caller prints it and exits with its code.
pub fn run(command: &ConfigCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        ConfigCommand::Path => print(&ctx.profile_dir()?.config_file().path().display()),
        ConfigCommand::Show { effective } => show(ctx, *effective),
        ConfigCommand::Get { key } => get(ctx, key),
        ConfigCommand::Set { key, value } => {
            ctx.profile_dir()?.config_file().set(key, value)?;
            Ok(0)
        }
        ConfigCommand::Unset { key } => {
            if !ctx.profile_dir()?.config_file().unset(key)? {
                eprintln!("{key} was not set; its default applies");
            }
            Ok(0)
        }
        ConfigCommand::Edit => edit::run(&ctx.profile_dir()?, ctx.launch),
        ConfigCommand::Validate => validate(ctx),
        ConfigCommand::Explain { key, changed } => match (key, changed) {
            (Some(key), _) => explain::key(&ctx.scope()?, key, ctx.json),
            (None, _) => explain::changed(&ctx.scope()?, ctx.json),
        },
        ConfigCommand::Schema => print_raw(&catalog::pretty(catalog::schema())),
        ConfigCommand::Docs => print_raw(&reference::render(
            catalog::schema(),
            "",
            "Configuration reference",
            "Every key a profile's `config.toml` accepts.",
        )),
    }
}

fn show(ctx: &Context<'_>, effective: bool) -> Result<u8, Error> {
    #[derive(Serialize)]
    struct Shown<'a> {
        profile: &'a str,
        path: String,
        effective: bool,
        config: Value,
    }
    let scope = ctx.scope()?;
    let path = scope.profile().config_file().path().to_owned();
    if !ctx.json {
        let text = if effective {
            toml::to_string(scope.config()).expect("a config serializes to TOML")
        } else {
            scope.profile().config_file().read()?.0
        };
        return print_raw(&text);
    }
    let config = if effective {
        serde_json::to_value(scope.config()).expect("a config serializes to JSON")
    } else {
        // The scope just validated this text, so it parses.
        let text = scope.profile().config_file().read()?.0;
        let table: toml::Table = toml::from_str(&text).expect("validated TOML parses");
        serde_json::to_value(table).expect("a TOML table serializes to JSON")
    };
    let shown = Shown {
        profile: scope.name().as_str(),
        path: path.display().to_string(),
        effective,
        config,
    };
    document("config show", &shown)
}

fn get(ctx: &Context<'_>, key: &str) -> Result<u8, Error> {
    #[derive(Serialize)]
    struct Got<'a> {
        key: &'a str,
        value: Option<&'a Value>,
        source: String,
    }
    let info = known_key(key)?;
    let scope = ctx.scope()?;
    let value = scope.layers().value(key);
    if ctx.json {
        let source = scope.layers().source(key).to_string();
        return document("config get", &Got { key, value, source });
    }
    match value {
        Some(value) => print(&explain::render_value(value)),
        None if info.section => print(&""),
        None => {
            eprintln!("{key} is unset");
            Ok(0)
        }
    }
}

fn validate(ctx: &Context<'_>) -> Result<u8, Error> {
    let profile = ctx.profile_dir()?;
    let file = profile.config_file();
    let (text, _) = file.read()?;
    pmt_config::check(file.path(), &text)?;
    // Backend names are a registry, not a schema enum (plugins add names).
    let scope = ctx.scope()?;
    let backend = scope.config().terminal.backend.clone();
    if let Err(err) = pmt_sandbox::backend::BackendRegistry::builtin().select(&backend)
        && err.code() == &codes::TERMINAL_UNKNOWN_BACKEND
    {
        return Err(err);
    }
    // Provider blocks and the fallback chain name registry entries
    // (custom blocks need a base URL; chain entries a known provider).
    let providers = pmt_providers::ProviderRegistry::for_profile(scope.config(), Vec::new())?;
    pmt_providers::fallback::chain(scope.config(), &providers)?;
    // Model aliases name registry providers; reasoning overrides name
    // levels or tiers.
    for (name, alias) in &scope.config().model.aliases {
        let target = pmt_providers::target::alias(&scope.config().model, name);
        let provider = target.map_or_else(|| alias_ref(alias), |t| t.provider);
        providers.lookup(&provider).map_err(|err| {
            Error::new(
                err.code(),
                format!("model.aliases.{name}: {}", err.message()),
            )
        })?;
    }
    pmt_providers::reasoning::check(&scope.config().agent)?;
    if !ctx.json {
        eprintln!("{} is valid", file.path().display());
    }
    Ok(0)
}

/// The text of an alias that is not `<provider>/<model>` (reported as an
/// unknown provider).
fn alias_ref(alias: &pmt_config::schema::ModelAlias) -> String {
    match alias {
        pmt_config::schema::ModelAlias::Ref(text) => text.clone(),
        pmt_config::schema::ModelAlias::Route(route) => route.provider.clone(),
    }
}

/// The catalog entry of `key`, or a usage error with suggestions.
fn known_key(key: &str) -> Result<catalog::KeyInfo, Error> {
    catalog::lookup(key).ok_or_else(|| {
        Error::new(
            &codes::CONFIG_INVALID_ARGUMENT,
            pmt_config::validate::unknown_key(key, None).message,
        )
        .with_hint("list keys with: prometheus config docs")
    })
}

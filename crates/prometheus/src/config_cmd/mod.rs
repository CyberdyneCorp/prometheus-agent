//! `prometheus config` (spec: configuration-and-profiles, Config CLI): every
//! read goes through a [`ProfileScope`], every write through the one
//! format-preserving writer, and the schema, docs and `explain` come from
//! the typed schema.

pub mod args;

pub(crate) mod edit;
mod explain;

use std::fmt::Write as _;

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
        #[serde(skip_serializing_if = "Option::is_none")]
        managed: Option<ManagedShown>,
    }
    let scope = ctx.scope()?;
    let path = scope.profile().config_file().path().to_owned();
    let managed = pmt_config::managed::current()?.filter(|m| m.config_loaded());
    if !ctx.json {
        let text = if effective {
            toml::to_string(scope.config()).expect("a config serializes to TOML")
        } else {
            scope.profile().config_file().read()?.0
        };
        let header = managed.map(managed_header).unwrap_or_default();
        return print_raw(&format!("{header}{text}"));
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
        managed: managed.map(ManagedShown::of),
    };
    document("config show", &shown)
}

/// The managed layer in `config show --json`.
#[derive(Serialize)]
struct ManagedShown {
    path: String,
    keys: Vec<ManagedKey>,
}

#[derive(Serialize)]
struct ManagedKey {
    key: String,
    locked: bool,
}

impl ManagedShown {
    fn of(managed: &pmt_config::managed::Managed) -> Self {
        Self {
            path: managed.config_path().display().to_string(),
            keys: managed_keys(managed)
                .into_iter()
                .map(|(key, locked)| ManagedKey { key, locked })
                .collect(),
        }
    }
}

/// Every managed or locked key, with whether it is locked.
fn managed_keys(managed: &pmt_config::managed::Managed) -> Vec<(String, bool)> {
    let mut keys: std::collections::BTreeMap<String, bool> = managed
        .values()
        .iter()
        .map(|(key, _)| (key.clone(), false))
        .collect();
    for key in managed.locked() {
        keys.insert(key.clone(), true);
    }
    keys.into_iter().collect()
}

/// The comment header naming the administrator's layer (spec:
/// configuration-and-profiles, Managed secrets and managed visibility).
fn managed_header(managed: &pmt_config::managed::Managed) -> String {
    let mut header = format!(
        "# managed by your administrator: {}\n",
        managed.config_path().display()
    );
    for (key, locked) in managed_keys(managed) {
        let suffix = if locked { " (locked)" } else { "" };
        let _ = writeln!(header, "#   {key}{suffix}");
    }
    header.push('\n');
    header
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
    // Toolset names are a registry as well (plugins and MCP servers add
    // names); M1 knows the built-in toolsets.
    let builtins = std::sync::Arc::new(pmt_tools::Builtins::standard(Vec::new())?);
    let tools = pmt_tools::ToolRegistry::new(scope.name().as_str(), builtins);
    let config = scope.config();
    pmt_tools::selection::validate(&tools, &config.tools, &config.toolsets)?;
    // Cloud browser providers are plugins (M2); M1 knows `local` only.
    pmt_tools::browser::validate_provider(&config.browser, &[])?;
    // Provider blocks and the fallback chain name registry entries
    // (custom blocks need a base URL; chain entries a known provider,
    // plugin providers included).
    let (providers, _) = crate::plugins_host::providers::registry(&scope, scope.config())?;
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
    // Context engines are a registry too: plugin engines arrive with the
    // plugin host (M2), so M1 knows the built-in engine only.
    pmt_agent::compress::engine::select(&scope.config().context, &[])?;
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
pub(crate) fn known_key(key: &str) -> Result<catalog::KeyInfo, Error> {
    catalog::lookup(key).ok_or_else(|| {
        Error::new(
            &codes::CONFIG_INVALID_ARGUMENT,
            pmt_config::validate::unknown_key(key, None).message,
        )
        .with_hint("list keys with: prometheus config docs")
    })
}

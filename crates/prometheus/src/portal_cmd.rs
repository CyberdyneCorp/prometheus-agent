//! `prometheus portal status|open|tools` and `prometheus setup --portal`
//! (spec: model-providers, Subscription tool gateway routing): the Nous
//! Portal subscription, and which tool categories go through its tool
//! gateway.

use std::fmt::Write as _;

use clap::Subcommand;
use pmt_core::error::{Error, codes};
use pmt_providers::auth::gateway::{Category, GATEWAY_PROVIDER, PORTAL_URL, RouteView, Routing};
use serde::Serialize;

use crate::context::Context;
use crate::output::{json, print_raw};

/// `prometheus portal` subcommands.
#[derive(Debug, Subcommand)]
pub enum PortalCommand {
    /// The portal sign-in and the tool gateway routing.
    Status,
    /// Open (or print) the portal's plan page.
    Open,
    /// Show the tool categories' routing, or route categories through the
    /// gateway (`--route web_search --route tts`, or `--route all`).
    Tools {
        /// A category to route through the gateway (repeatable; `all`).
        #[arg(long, value_name = "CATEGORY")]
        route: Vec<String>,
    },
}

#[derive(Serialize)]
struct StatusView {
    provider: &'static str,
    signed_in: Option<String>,
    plan: Option<String>,
    expires_at: Option<String>,
    gateway_domain: String,
    tools: Vec<RouteView>,
}

/// Runs one `portal` subcommand.
///
/// # Errors
///
/// Config, secrets and store errors; `cli.usage` for an unknown category.
pub fn run(command: &PortalCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        PortalCommand::Status => status(ctx),
        PortalCommand::Open => open(ctx),
        PortalCommand::Tools { route } => tools(ctx, route),
    }
}

fn views(ctx: &Context<'_>) -> Result<Vec<RouteView>, Error> {
    let scope = ctx.scope()?;
    let config = scope.config();
    let credentials = pmt_providers::auth::Credentials::new(scope.secrets().store().clone());
    let routing = Routing {
        config,
        secrets: scope.secrets(),
        extras: &credentials,
    };
    Ok(routing.views())
}

fn status(ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let credentials = pmt_providers::auth::Credentials::new(scope.secrets().store().clone());
    let sub = credentials
        .subscriptions()?
        .into_iter()
        .find(|s| s.provider == GATEWAY_PROVIDER);
    let view = StatusView {
        provider: GATEWAY_PROVIDER,
        signed_in: sub.as_ref().map(|s| s.label.clone()),
        plan: sub.as_ref().and_then(|s| s.plan.clone()),
        expires_at: sub.as_ref().and_then(|s| s.expires_at.clone()),
        gateway_domain: scope.config().tool_gateway.domain.clone(),
        tools: views(ctx)?,
    };
    if ctx.json {
        return json(&view);
    }
    let mut text = match &view.signed_in {
        Some(label) => format!(
            "Nous Portal: signed in as '{label}'{}\n",
            view.plan
                .as_deref()
                .map(|p| format!(" (plan {p})"))
                .unwrap_or_default()
        ),
        None => "Nous Portal: not signed in (prometheus setup --portal)\n".to_owned(),
    };
    let _ = writeln!(text, "tool gateway: *.{}", view.gateway_domain);
    text.push_str(&render(&view.tools));
    print_raw(&text)
}

fn render(tools: &[RouteView]) -> String {
    let mut text = String::new();
    for view in tools {
        let _ = writeln!(
            text,
            "{:<17} {:<8} {} ({} = {})",
            view.category.name(),
            view.route,
            view.detail,
            view.key,
            view.selection.as_deref().unwrap_or("unset")
        );
    }
    text
}

fn open(ctx: &Context<'_>) -> Result<u8, Error> {
    let url = format!("{PORTAL_URL}/plans");
    if ctx.json {
        return json(&serde_json::json!({ "url": url }));
    }
    print_raw(&format!("{url}\n"))
}

fn tools(ctx: &Context<'_>, route: &[String]) -> Result<u8, Error> {
    if !route.is_empty() {
        let scope = ctx.scope()?;
        for category in categories(route)? {
            route_through_gateway(&scope, category)?;
        }
    }
    let tools = views(ctx)?;
    if ctx.json {
        return json(&tools);
    }
    print_raw(&render(&tools))
}

/// Points `category`'s selection at the gateway provider.
fn route_through_gateway(
    scope: &pmt_config::ProfileScope,
    category: Category,
) -> Result<(), Error> {
    let file = scope.profile().config_file();
    file.set(category.key(), GATEWAY_PROVIDER)
}

/// The categories `names` lists (`all` for every one).
fn categories(names: &[String]) -> Result<Vec<Category>, Error> {
    if names.iter().any(|n| n == "all") {
        return Ok(Category::ALL.to_vec());
    }
    names
        .iter()
        .map(|name| {
            Category::parse(name).ok_or_else(|| {
                let known: Vec<&str> = Category::ALL.iter().map(|c| c.name()).collect();
                Error::new(
                    &codes::CLI_USAGE,
                    format!(
                        "unknown tool category '{name}' (known: {}, all)",
                        known.join(", ")
                    ),
                )
            })
        })
        .collect()
}

/// The tool categories `setup --portal` sends through the tool gateway:
/// web, image and speech (spec: cli, Setup wizard modes).
const PORTAL_CATEGORIES: [Category; 4] = [
    Category::WebSearch,
    Category::WebExtract,
    Category::ImageGeneration,
    Category::Tts,
];

/// The `(key, value)` pairs `setup --portal` writes: `nous` with the
/// portal's default model as the main route, and the web, image and TTS
/// providers through its gateway.
fn portal_pairs(scope: &pmt_config::ProfileScope) -> Result<Vec<(&'static str, String)>, Error> {
    let registry = pmt_providers::ProviderRegistry::for_profile(scope.config(), Vec::new())?;
    let default_model = registry
        .lookup(GATEWAY_PROVIDER)?
        .descriptor()
        .default_model
        .clone();
    let mut pairs = vec![("model.provider", GATEWAY_PROVIDER.to_owned())];
    if let Some(model) = default_model {
        pairs.push(("model.default", model));
    }
    pairs.extend(
        PORTAL_CATEGORIES
            .iter()
            .map(|c| (c.key(), GATEWAY_PROVIDER.to_owned())),
    );
    Ok(pairs)
}

/// The lines of the change `pairs` make (`key: old -> new`), unchanged
/// keys left out.
fn diff(scope: &pmt_config::ProfileScope, pairs: &[(&str, String)]) -> Vec<String> {
    pairs
        .iter()
        .filter_map(|(key, new)| {
            let old = scope.layers().value(key).map(crate::setup_cmd::plain);
            (old.as_deref() != Some(new.as_str()))
                .then(|| format!("  {key}: {} -> {new}", old.as_deref().unwrap_or("unset")))
        })
        .collect()
}

/// `prometheus setup --portal`: signs in to the Nous Portal, then shows
/// the change (main route `nous` with its default model; web, image and
/// speech tools through the tool gateway) and writes it once confirmed
/// (a run without a terminal writes what it showed).
///
/// # Errors
///
/// Sign-in and config write errors.
pub fn setup(ctx: &Context<'_>) -> Result<u8, Error> {
    let sub = crate::auth_cmd::sign_in_default(ctx, GATEWAY_PROVIDER)?;
    eprintln!("signed in to the Nous Portal as '{}'", sub.label);
    let scope = ctx.scope()?;
    let pairs = portal_pairs(&scope)?;
    let lines = diff(&scope, &pairs);
    if lines.is_empty() {
        eprintln!("the profile already uses the Nous Portal; nothing to change");
        return Ok(0);
    }
    eprintln!("config.toml changes:\n{}", lines.join("\n"));
    if crate::prompt::stdin_is_tty() && !crate::prompt::confirm("Write these changes?")? {
        eprintln!("nothing written");
        return Ok(1);
    }
    let pairs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    scope.profile().config_file().set_all(&pairs)?;
    eprintln!("the main route and the web, image and speech tools now use the Nous Portal");
    Ok(0)
}

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

/// `prometheus setup --portal`: signs in to the Nous Portal, makes `nous`
/// the main provider and, when the user agrees (never without a
/// terminal), routes every tool category through the gateway.
///
/// # Errors
///
/// Sign-in and config write errors.
pub fn setup(ctx: &Context<'_>) -> Result<u8, Error> {
    let sub = crate::auth_cmd::sign_in_default(ctx, GATEWAY_PROVIDER)?;
    let scope = ctx.scope()?;
    let file = scope.profile().config_file();
    file.set("model.provider", GATEWAY_PROVIDER)?;
    eprintln!(
        "signed in to the Nous Portal as '{}'; the main provider is now nous",
        sub.label
    );
    let route_all = crate::prompt::stdin_is_tty()
        && crate::prompt::confirm(
            "Route web search, extraction, image and video generation, speech, the cloud browser and remote sandboxes through the Nous tool gateway?",
        )?;
    if route_all {
        for category in Category::ALL {
            route_through_gateway(&scope, category)?;
        }
        eprintln!("every tool category now goes through the Nous tool gateway");
    }
    Ok(0)
}

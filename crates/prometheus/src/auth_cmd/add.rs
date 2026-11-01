//! `prometheus auth add <provider> [--type api_key|oauth] [--label L]
//! [--browser|--device]`.

use pmt_config::schema::ProviderAuth;
use pmt_core::error::{Error, codes};
use pmt_providers::auth::browser::{CALLBACK_TIMEOUT, Place};
use pmt_providers::auth::credentials::{api_key_secret, check_label};
use pmt_providers::auth::{self, DEFAULT_LABEL, FlowChoice};
use pmt_providers::registry::Provider;
use serde::Serialize;

use super::{Profile, TerminalUi};
use crate::cli::AuthTypeArg;
use crate::context::Context;
use crate::model_cmd::{block_on, client};
use crate::output::json;
use crate::prompt;

/// The parsed arguments.
pub(super) struct Args<'a> {
    pub provider: &'a str,
    pub kind: Option<AuthTypeArg>,
    pub label: Option<&'a str>,
    pub browser: bool,
    pub device: bool,
    pub argv_key: &'a [String],
}

#[derive(Serialize)]
struct Added<'a> {
    profile: &'a str,
    provider: &'a str,
    label: &'a str,
    kind: &'static str,
    source: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<&'a str>,
}

pub(super) fn run(ctx: &Context<'_>, args: &Args<'_>) -> Result<u8, Error> {
    if !args.argv_key.is_empty() {
        return Err(Error::new(
            &codes::AUTH_KEY_ON_ARGV,
            "API keys are never accepted as arguments; they are read from the hidden prompt or stdin",
        )
        .with_hint(format!(
            "run `prometheus auth add {}` and type the key at the prompt, or pipe it on stdin",
            args.provider
        )));
    }
    let label = args.label.unwrap_or(DEFAULT_LABEL);
    check_label(label)?;
    let profile = Profile::open(ctx)?;
    let provider = profile.registry.lookup(args.provider)?;
    let oauth = match args.kind {
        Some(AuthTypeArg::Oauth) => true,
        Some(AuthTypeArg::ApiKey) => false,
        None => args.browser || args.device || provider.auth() == ProviderAuth::Oauth,
    };
    if oauth {
        let choice = match (args.browser, args.device) {
            (true, _) => FlowChoice::Browser,
            (_, true) => FlowChoice::Device,
            _ => FlowChoice::Default,
        };
        return sign_in(ctx, &profile, provider, label, choice);
    }
    if args.browser || args.device {
        return Err(Error::new(
            &codes::AUTH_UNSUPPORTED,
            "--browser and --device choose a sign-in flow; they do not apply to --type api_key",
        ));
    }
    api_key(ctx, &profile, provider, label)
}

fn api_key(
    ctx: &Context<'_>,
    profile: &Profile,
    provider: &Provider,
    label: &str,
) -> Result<u8, Error> {
    let id = provider.id();
    let name = api_key_secret(provider, label).ok_or_else(|| {
        Error::new(
            &codes::AUTH_UNSUPPORTED,
            format!("provider '{id}' takes no API key"),
        )
        .with_hint(format!(
            "sign in instead: prometheus auth add {id} --type oauth"
        ))
    })?;
    let value = prompt::read_secret(&name)?;
    profile.scope.secrets().store().set(&name, &value)?;
    let added = Added {
        profile: profile.scope.name().as_str(),
        provider: id,
        label,
        kind: "api_key",
        source: "store",
        secret: Some(&name),
        plan: None,
    };
    if ctx.json {
        return json(&added);
    }
    eprintln!("stored API key '{label}' of {id} as secret {name}");
    Ok(0)
}

fn sign_in(
    ctx: &Context<'_>,
    profile: &Profile,
    provider: &Provider,
    label: &str,
    choice: FlowChoice,
) -> Result<u8, Error> {
    let sub = sign_in_and_save(ctx, profile, provider, label, choice)?;
    let added = Added {
        profile: profile.scope.name().as_str(),
        provider: provider.id(),
        label,
        kind: "subscription",
        source: &sub.source,
        secret: None,
        plan: sub.plan.as_deref(),
    };
    if ctx.json {
        return json(&added);
    }
    let plan = sub
        .plan
        .as_deref()
        .map(|p| format!(" (plan {p})"))
        .unwrap_or_default();
    eprintln!("signed in to {} as '{label}'{plan}", provider.id());
    Ok(0)
}

/// Signs in to provider `id` on the terminal with its default flow and
/// stores the sign-in as label `default` (the model picker's subscription
/// choice).
///
/// # Errors
///
/// As `prometheus auth add <id>`.
pub(crate) fn sign_in_default(ctx: &Context<'_>, id: &str) -> Result<auth::Subscription, Error> {
    let profile = Profile::open(ctx)?;
    let provider = profile.registry.lookup(id)?;
    sign_in_and_save(ctx, &profile, provider, DEFAULT_LABEL, FlowChoice::Default)
}

/// Runs `provider`'s sign-in on the terminal and stores it as `label`.
fn sign_in_and_save(
    ctx: &Context<'_>,
    profile: &Profile,
    provider: &Provider,
    label: &str,
    choice: FlowChoice,
) -> Result<auth::Subscription, Error> {
    let remote = ctx.launch.is_ssh_session();
    let place = Place {
        remote,
        can_open_browser: !remote && ctx.launch.has_graphical_display() && prompt::stdin_is_tty(),
        timeout: CALLBACK_TIMEOUT,
    };
    let ui = TerminalUi { launch: ctx.launch };
    let http = client(profile.scope.config())?;
    let done = block_on(auth::sign_in(&http, provider, choice, &ui, place))??;
    profile
        .credentials
        .save(provider.id(), label, &done.tokens, &done.obtained)
}

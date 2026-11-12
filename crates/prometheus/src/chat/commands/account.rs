//! Subscription account commands (spec: model-providers, Subscription
//! account commands; Account usage windows command): `/usage` with the
//! account block, session duration and context window, `/usage reset`,
//! `/login`, `/subscription` (`/upgrade`) and `/topup`.

use std::fmt::Write as _;
use std::sync::Arc;

use pmt_agent::turn::model::ModelPort;
use pmt_core::error::{Error, codes};
use pmt_i18n::t;
use pmt_providers::account::AccountUsage;
use pmt_providers::auth::{self, Credentials, FlowChoice, SignInUi};
use pmt_providers::registry::Provider;

use super::{Ctx, Post};

/// The provider `/login` signs in to by default.
const DEFAULT_LOGIN: &str = "nous";

/// `/usage [reset [--force]]`.
pub(super) async fn usage(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let words: Vec<&str> = args.split_whitespace().collect();
    match words.as_slice() {
        [] => {}
        ["reset"] => return reset(ctx, false).await,
        ["reset", "--force"] | ["--force", "reset"] => return reset(ctx, true).await,
        _ => {
            return Err(Error::new(
                &codes::CLI_USAGE,
                "usage: /usage [reset [--force]]",
            ));
        }
    }
    let strings = ctx.strings();
    let mut text = super::session::usage(ctx)?;
    let session = ctx.factory.store.session(ctx.live.id)?;
    let elapsed = jiff::Timestamp::now().duration_since(session.started_at);
    let line = t!(
        strings,
        "slash.usage.session",
        duration = duration(&strings, elapsed.as_secs())
    );
    let _ = write!(text, "\n{line}");
    if let Some(line) = context_line(ctx).await? {
        let _ = write!(text, "\n{line}");
    }
    if let Some(account) = account_block(ctx) {
        let _ = write!(text, "\n{account}");
    }
    Ok(text)
}

fn duration(strings: &pmt_i18n::Strings, secs: i64) -> String {
    let secs = secs.max(0);
    match (secs / 3600, (secs % 3600) / 60) {
        (0, m) => t!(
            strings,
            "slash.usage.minutes",
            minutes = m,
            seconds = secs % 60
        ),
        (h, m) => t!(strings, "slash.usage.hours", hours = h, minutes = m),
    }
}

/// `context <used> / <window> tokens (<n>%)` from the newest main call.
/// While a turn runs its call holds the model route, and a busy `/usage`
/// runs inside the turn's own loop: waiting for the route would never
/// end, so the window then comes from the catalog entry of that call's
/// model.
async fn context_line(ctx: &Ctx<'_>) -> Result<Option<String>, Error> {
    let last = ctx
        .factory
        .store
        .usage_events(ctx.live.id)?
        .into_iter()
        .rev()
        .find(|e| e.purpose == "main");
    let Some(last) = last else {
        return Ok(None);
    };
    let window = if ctx.running {
        let catalog = ctx.factory.models.catalog();
        Some(
            catalog
                .metadata_at(&last.provider, &last.model, None)
                .0
                .context_window,
        )
    } else {
        ctx.live.model.route().await.context_window
    };
    let Some(window) = window else {
        return Ok(None);
    };
    let used = last.tokens.input + last.tokens.cache_read + last.tokens.cache_write;
    let percent = used.saturating_mul(100) / window.max(1);
    Ok(Some(t!(
        ctx.strings(),
        "slash.usage.context",
        used = used,
        window = window,
        percent = percent
    )))
}

/// The account block of the session's subscription, from the stored quota
/// snapshot (the status line keeps it fresh).
fn account_block(ctx: &Ctx<'_>) -> Option<String> {
    let store = &ctx.factory.store;
    let last = store.usage_events(ctx.live.id).ok()?.pop()?;
    let (_, quota) =
        pmt_providers::quota::for_session(store, &last.provider, &last.billing_source)?;
    let strings = ctx.strings();
    let usage = match quota {
        Some(quota) => AccountUsage::from_quota(&last.provider, &last.billing_source, &quota),
        None => AccountUsage::unavailable(&last.provider, strings.get("slash.usage.not_reported")),
    };
    Some(t!(
        strings,
        "slash.usage.account",
        usage = usage.render_in(&*strings)
    ))
}

/// `/usage reset [--force]`: redeems a banked limit reset of the session's
/// ChatGPT plan, after a confirmation (`--force` skips it).
async fn reset(ctx: &Ctx<'_>, force: bool) -> Result<String, Error> {
    let route = ctx.live.model.route().await;
    let strings = ctx.strings();
    let Some(label) = route
        .billing_source
        .strip_prefix(&format!("subscription:{}/", route.provider))
    else {
        return Ok(t!(
            strings,
            "slash.usage.reset_no_subscription",
            provider = route.provider
        ));
    };
    if !pmt_providers::quota::resets(&route.provider) {
        return Ok(t!(
            strings,
            "slash.usage.reset_unavailable",
            provider = route.provider
        ));
    }
    if !force {
        return Ok(t!(
            strings,
            "slash.usage.reset_confirm",
            provider = route.provider,
            label = label
        ));
    }
    let resolver = ctx.factory.models.resolver();
    let provider = resolver.registry.lookup(&route.provider)?;
    let credentials = Credentials::new(ctx.scope.secrets().store().clone());
    let sub = credentials
        .subscription(provider.id(), label)?
        .ok_or_else(|| Error::new(&codes::AUTH_NOT_FOUND, format!("no sign-in '{label}'")))?;
    let token = credentials
        .access_token(&sub)?
        .ok_or_else(|| pmt_providers::auth::reauth_required(provider.id(), label))?;
    let client = crate::model_cmd::client(ctx.scope.config())?;
    let base_url = provider.base_url().unwrap_or_default();
    pmt_providers::quota::redeem_reset(
        &client,
        provider.id(),
        base_url,
        &token,
        sub.account_id.as_deref(),
    )
    .await?;
    Ok(t!(
        strings,
        "slash.usage.reset_done",
        provider = provider.id()
    ))
}

/// Sign-in messages posted to the session as they come.
struct Posted(Post);

impl SignInUi for Posted {
    fn say(&self, text: &str) {
        (self.0)(text.to_owned());
    }
}

/// `/login [provider]`: the device-code sign-in runs off-turn; its code and
/// the outcome are posted to the session (a gateway only gets here from a
/// direct message).
pub(super) fn login(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let name = args.split_whitespace().next().unwrap_or(DEFAULT_LOGIN);
    let provider = ctx.factory.models.resolver().registry.lookup(name)?.clone();
    let strings = ctx.strings();
    if provider.login_flows().is_empty() {
        return Ok(t!(
            strings,
            "slash.login.no_sign_in",
            provider = provider.id()
        ));
    }
    let client = crate::model_cmd::client(ctx.scope.config())?;
    let credentials = Credentials::new(ctx.scope.secrets().store().clone());
    let post = Arc::clone(&ctx.post);
    let said = Arc::clone(&strings);
    pmt_core::spawn::spawn(async move {
        let ui = Posted(Arc::clone(&post));
        let place = auth::browser::Place {
            remote: true,
            can_open_browser: false,
            timeout: auth::browser::CALLBACK_TIMEOUT,
        };
        let done = auth::sign_in(&client, &provider, FlowChoice::Device, &ui, place).await;
        let outcome = done.and_then(|done| {
            credentials.save(
                provider.id(),
                auth::DEFAULT_LABEL,
                &done.tokens,
                &done.obtained,
            )
        });
        let id = provider.id();
        post(match outcome {
            Ok(sub) => match sub.plan {
                Some(plan) => t!(
                    said,
                    "slash.login.signed_in_plan",
                    provider = id,
                    plan = plan
                ),
                None => t!(said, "slash.login.signed_in", provider = id),
            },
            Err(err) => t!(
                said,
                "slash.login.failed",
                provider = id,
                why = err.message()
            ),
        });
    });
    Ok(t!(strings, "slash.login.started", provider = name))
}

/// The provider whose account `/subscription` and `/topup` show: the
/// session's when it bills a subscription, else the first signed-in
/// provider with account pages.
async fn account_provider(ctx: &Ctx<'_>) -> Result<Option<Provider>, Error> {
    let route = ctx.live.model.route().await;
    let resolver = ctx.factory.models.resolver();
    let registry = resolver.registry;
    if route.billing_source.starts_with("subscription:") {
        return Ok(registry.lookup(&route.provider).ok().cloned());
    }
    let credentials = Credentials::new(ctx.scope.secrets().store().clone());
    Ok(credentials
        .subscriptions()?
        .into_iter()
        .filter_map(|s| registry.lookup(&s.provider).ok())
        .find(|p| p.descriptor().account_urls.plan.is_some())
        .cloned())
}

/// `/subscription` (`/upgrade`): the plan label and where to change it.
pub(super) async fn subscription(ctx: &Ctx<'_>) -> Result<String, Error> {
    let strings = ctx.strings();
    let Some(provider) = account_provider(ctx).await? else {
        return Ok(t!(strings, "slash.account.no_sign_in"));
    };
    let Some(url) = provider.descriptor().account_urls.plan.clone() else {
        return Ok(t!(
            strings,
            "slash.account.unavailable",
            provider = provider.id()
        ));
    };
    let plan = plan_of(ctx, &provider).unwrap_or_else(|| t!(strings, "slash.account.plan_unknown"));
    Ok(t!(
        strings,
        "slash.account.plan",
        title = pmt_providers::account::title(provider.id()),
        plan = plan,
        url = url
    ))
}

/// `/topup`: the prepaid balance where reported, and the billing page.
pub(super) async fn topup(ctx: &Ctx<'_>) -> Result<String, Error> {
    let strings = ctx.strings();
    let Some(provider) = account_provider(ctx).await? else {
        return Ok(t!(strings, "slash.account.no_sign_in"));
    };
    let Some(url) = provider.descriptor().account_urls.billing.clone() else {
        return Ok(t!(
            strings,
            "slash.account.unavailable",
            provider = provider.id()
        ));
    };
    Ok(t!(
        strings,
        "slash.account.topup",
        title = pmt_providers::account::title(provider.id()),
        url = url
    ))
}

fn plan_of(ctx: &Ctx<'_>, provider: &Provider) -> Option<String> {
    let credentials = Credentials::new(ctx.scope.secrets().store().clone());
    credentials
        .subscriptions()
        .ok()?
        .into_iter()
        .find(|s| s.provider == provider.id())
        .and_then(|s| s.plan)
}

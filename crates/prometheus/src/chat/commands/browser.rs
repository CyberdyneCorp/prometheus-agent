//! `/browser connect [<url>]|status|disconnect` (spec:
//! execution-environments, Connecting to the user's browser; `browser_cdp`
//! and `browser_dialog` tools).
//!
//! `connect` binds this session's browser to the user's own browser over
//! DevTools (starting one on port 9222 when nothing listens there) and
//! offers `browser_cdp` and `browser_dialog` from now on; `disconnect`
//! detaches without closing that browser and returns to the configured
//! provider. Offering or withdrawing the two tools rebuilds this
//! session's tool array, unless `browser.cdp_url` already offers them.
//! Only terminal surfaces have the command; messaging platforms refuse it.

use pmt_core::error::{Error, codes};
use pmt_tools::browser::connect::{self, Starter, Status};

use super::Ctx;

/// Runs `/browser`.
pub(super) async fn browser(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let words: Vec<&str> = args.split_whitespace().collect();
    match words.as_slice() {
        ["connect"] => connect(ctx, None).await,
        ["connect", url] => connect(ctx, Some(url)).await,
        [] | ["status"] => Ok(status(ctx).await),
        ["disconnect"] => disconnect(ctx).await,
        _ => Err(Error::new(
            &codes::CLI_USAGE,
            "usage: /browser connect [<ws-or-http-url>] | status | disconnect",
        )),
    }
}

fn manager(ctx: &Ctx<'_>) -> Result<std::sync::Arc<pmt_tools::browser::BrowserManager>, Error> {
    ctx.factory
        .browser()
        .map(|shared| std::sync::Arc::clone(shared.manager()))
        .ok_or_else(|| {
            Error::new(
                &codes::CLI_USAGE,
                "the browser tools are unavailable in this profile",
            )
        })
}

fn key(ctx: &Ctx<'_>) -> String {
    format!("{}/default", ctx.live.id)
}

/// Whether `browser.cdp_url` offers the connected-browser tools already.
fn configured_endpoint(ctx: &Ctx<'_>) -> bool {
    ctx.scope
        .config()
        .browser
        .cdp_url
        .as_deref()
        .is_some_and(|url| !url.trim().is_empty())
}

async fn connect(ctx: &Ctx<'_>, url: Option<&str>) -> Result<String, Error> {
    let manager = manager(ctx)?;
    let launch = ctx.factory.launch();
    let starter = Starter::new(
        connect::candidates(launch.inherited("PATH")),
        ctx.scope.path("cache/browser"),
        launch.has_graphical_display(),
    );
    let unreachable = |why: String| Error::new(&codes::CLI_USAGE, format!("cannot connect: {why}"));
    let endpoint = connect::resolve(url, &starter).await.map_err(unreachable)?;
    manager.set_connect(&ctx.live.id.to_string(), Some(endpoint.clone()));
    let status = match Status::read(&endpoint).await {
        Ok(status) => status.render(),
        Err(why) => format!("connected to {endpoint} ({why})"),
    };
    if !configured_endpoint(ctx) {
        ctx.factory
            .apply_tools_now(ctx.live, "browser connect")
            .await?;
        return Ok(format!(
            "{status}\nbrowser_cdp and browser_dialog are available now (the prompt cache starts over)"
        ));
    }
    Ok(status)
}

async fn status(ctx: &Ctx<'_>) -> String {
    let Ok(manager) = manager(ctx) else {
        return "the browser tools are unavailable in this profile".to_owned();
    };
    match manager.explicit_endpoint(&key(ctx)) {
        Some(endpoint) => match Status::read(&endpoint).await {
            Ok(status) => status.render(),
            Err(why) => format!("connected to {endpoint}, which does not answer: {why}"),
        },
        None => format!(
            "not connected; browser sessions use browser.provider = {}",
            manager.config().provider
        ),
    }
}

async fn disconnect(ctx: &Ctx<'_>) -> Result<String, Error> {
    let manager = manager(ctx)?;
    let session = ctx.live.id.to_string();
    if manager.explicit_endpoint(&key(ctx)).is_none() {
        return Ok("not connected".to_owned());
    }
    manager.set_connect(&session, None);
    // Detaches: a connected browser is never closed by its session's end.
    manager.close_prefixed(&format!("{session}/")).await;
    let provider = manager.config().provider;
    if configured_endpoint(ctx) {
        return Ok(
            "disconnected; browser.cdp_url still names an endpoint, so it is used next".to_owned(),
        );
    }
    ctx.factory
        .apply_tools_now(ctx.live, "browser disconnect")
        .await?;
    Ok(format!(
        "disconnected (the browser keeps running); the next browser call uses browser.provider = {provider}"
    ))
}

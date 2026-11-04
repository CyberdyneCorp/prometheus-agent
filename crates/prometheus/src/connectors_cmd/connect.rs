//! `prometheus connectors connect <app> [--alias] [--reconnect [--force]]`:
//! starts (or repairs) an authorization, prints its link and waits for the
//! account to become active.

use std::time::Duration;

use pmt_core::error::{Error, codes};
use pmt_tools::connectors::Gateway;
use pmt_tools::connectors::wire::{ConnectRequest, ConnectResult};
use serde_json::json;

use super::{ConnectArgs, gateway_error, matching};
use crate::context::Context;

/// How often the wait checks the account.
const POLL: Duration = Duration::from_secs(2);

pub(super) async fn run(
    ctx: &Context<'_>,
    gateway: &Gateway,
    args: &ConnectArgs,
    timeout: Duration,
) -> Result<u8, Error> {
    let Some(request) = request(ctx, gateway, args).await? else {
        return Ok(0);
    };
    let results = gateway
        .connect(&request)
        .await
        .map_err(|e| gateway_error(&e))?;
    let Some(result) = results.into_iter().next() else {
        return Err(Error::new(
            &codes::CONNECTORS_GATEWAY,
            "the gateway started no connection",
        ));
    };
    match (result.status.as_str(), &result.connect_url) {
        ("active", _) => Ok(done(ctx, &result, "already connected")),
        ("initiated", Some(url)) => {
            if ctx.json {
                eprintln!("open this link to connect {}: {url}", result.connector);
            } else {
                println!("Open this link to connect {}:\n  {url}", result.connector);
                println!("Waiting for the account (Ctrl-C stops waiting)...");
            }
            wait(ctx, gateway, &result, timeout).await
        }
        _ => Err(Error::new(
            &codes::CONNECTORS_GATEWAY,
            format!(
                "connecting {} failed: {}",
                result.connector,
                result.status_reason.as_deref().unwrap_or(&result.status)
            ),
        )),
    }
}

/// The connection request; `None` when a reconnect has nothing to do.
async fn request(
    ctx: &Context<'_>,
    gateway: &Gateway,
    args: &ConnectArgs,
) -> Result<Option<ConnectRequest>, Error> {
    if !args.reconnect {
        return Ok(Some(ConnectRequest {
            connectors: vec![args.app.clone()],
            alias: args.alias.clone(),
            ..ConnectRequest::default()
        }));
    }
    let accounts = gateway.accounts().await.map_err(|e| gateway_error(&e))?;
    let own = matching(&accounts, &args.app, args.alias.as_deref());
    let target = own
        .iter()
        .find(|a| !a.is_active())
        .or_else(|| own.first().filter(|_| args.force));
    let Some(account) = target else {
        if own.is_empty() {
            return Err(Error::new(
                &codes::CONNECTORS_NOT_FOUND,
                format!("no {} account to reconnect", args.app),
            )
            .with_hint(format!(
                "connect one with: prometheus connectors connect {}",
                args.app
            )));
        }
        let name = own[0].display_name();
        if ctx.json {
            println!(
                "{}",
                json!({"connector": args.app, "account": name, "status": "connected"})
            );
        } else {
            println!(
                "{} account '{name}' is connected; nothing to do (--force switches it)",
                args.app
            );
        }
        return Ok(None);
    };
    Ok(Some(ConnectRequest {
        connectors: vec![args.app.clone()],
        reinitiate: true,
        alias: account.alias.clone(),
        connection_id: Some(account.connection_id.clone()),
    }))
}

/// Waits until the account is active, the timeout passes or Ctrl-C.
async fn wait(
    ctx: &Context<'_>,
    gateway: &Gateway,
    result: &ConnectResult,
    timeout: Duration,
) -> Result<u8, Error> {
    let connected = async {
        loop {
            tokio::time::sleep(POLL).await;
            if let Ok(accounts) = gateway.accounts().await
                && accounts.iter().any(|a| {
                    a.is_active()
                        && match &result.connection_id {
                            Some(id) => &a.connection_id == id,
                            None => a.connector == result.connector && a.alias == result.alias,
                        }
                })
            {
                return;
            }
        }
    };
    tokio::select! {
        outcome = tokio::time::timeout(timeout, connected) => match outcome {
            Ok(()) => Ok(done(ctx, result, "connected")),
            Err(_) => Err(Error::new(
                &codes::CONNECTORS_NOT_CONNECTED,
                format!("{} was not connected within {}s", result.connector, timeout.as_secs()),
            )),
        },
        _ = tokio::signal::ctrl_c() => {
            eprintln!("stopped waiting; the link stays valid for a while");
            Ok(pmt_core::exit::INTERRUPTED)
        }
    }
}

fn done(ctx: &Context<'_>, result: &ConnectResult, state: &str) -> u8 {
    if ctx.json {
        println!(
            "{}",
            json!({"connector": result.connector, "alias": result.alias, "status": "connected"})
        );
    } else {
        let alias = result
            .alias
            .as_ref()
            .map(|a| format!(" (account '{a}')"))
            .unwrap_or_default();
        println!("{}{alias}: {state}", result.connector);
    }
    0
}

//! `prometheus connectors list|accounts|connect|rename|disconnect` (spec:
//! connectors, Connector account management CLI).
//!
//! Every command talks to the Nous connector gateway with the profile's
//! Nous sign-in (exit 78 without one). `connect` prints the authorization
//! link and waits for the account (Ctrl-C stops waiting, exit 130);
//! `disconnect` asks for confirmation unless `--yes` (without a terminal
//! it exits 4 and removes nothing).

mod connect;

use clap::{Args, Subcommand};
use pmt_core::error::{Error, codes};
use pmt_tools::connectors::manage::account_row;
use pmt_tools::connectors::names::is_alias;
use pmt_tools::connectors::wire::Account;
use pmt_tools::connectors::{Gateway, GatewayError};
use serde_json::json;

use crate::context::Context;

/// `prometheus connectors` subcommands.
#[derive(Debug, Subcommand)]
pub enum ConnectorsCommand {
    /// The connectors this account may use, with their connected state.
    List,
    /// Connector accounts with their names and states.
    Accounts {
        /// Only this connector's accounts.
        app: Option<String>,
        /// Include retired accounts (replaced by a reconnect).
        #[arg(long)]
        all: bool,
    },
    /// Connect an account: prints the link to open and waits until it is
    /// connected (or `connectors.connect_timeout_secs`; Ctrl-C stops
    /// waiting).
    Connect(ConnectArgs),
    /// Rename an account (by its name or label).
    Rename {
        /// The connector.
        app: String,
        /// The account's current name or label.
        account: String,
        /// The new name.
        #[arg(long)]
        to: String,
    },
    /// Remove an account (asks for confirmation unless --yes).
    Disconnect {
        /// The connector.
        app: String,
        /// The account's name (needed when the connector has several).
        #[arg(long)]
        alias: Option<String>,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
}

/// `connectors connect` flags.
#[derive(Debug, Args)]
pub struct ConnectArgs {
    /// The connector (`gmail`, `notion`...).
    pub app: String,
    /// Name the account.
    #[arg(long)]
    pub alias: Option<String>,
    /// Repair an account that is not connected (expired, failed, revoked).
    #[arg(long)]
    pub reconnect: bool,
    /// With --reconnect: also replace a connected account.
    #[arg(long, requires = "reconnect")]
    pub force: bool,
}

/// Runs `connectors <command>`.
///
/// # Errors
///
/// `connectors.*` errors (no sign-in 78, invalid alias 2, gateway 69...).
pub fn run(command: &ConnectorsCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    for alias in aliases(command) {
        if !is_alias(alias) {
            return Err(Error::new(
                &codes::CONNECTORS_INVALID_ALIAS,
                format!("invalid account name '{alias}'"),
            )
            .with_hint("use lower-case letters, digits and dashes (^[a-z0-9][a-z0-9-]{0,31}$)"));
        }
    }
    let scope = ctx.scope()?;
    let gateway = crate::connectors_host::gateway_or_error(&scope)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::new(&codes::TASK_PANICKED, format!("no async runtime: {e}")))?;
    let timeout = std::time::Duration::from_secs(scope.config().connectors.connect_timeout_secs);
    runtime.block_on(async {
        match command {
            ConnectorsCommand::List => list(ctx, &gateway).await,
            ConnectorsCommand::Accounts { app, all } => {
                accounts(ctx, &gateway, app.as_deref(), *all).await
            }
            ConnectorsCommand::Connect(args) => connect::run(ctx, &gateway, args, timeout).await,
            ConnectorsCommand::Rename { app, account, to } => {
                rename(ctx, &gateway, app, account, to).await
            }
            ConnectorsCommand::Disconnect { app, alias, yes } => {
                disconnect(ctx, &gateway, app, alias.as_deref(), *yes).await
            }
        }
    })
}

/// The aliases the command names (checked before anything is sent).
fn aliases(command: &ConnectorsCommand) -> Vec<&str> {
    match command {
        ConnectorsCommand::Connect(args) => args.alias.iter().map(String::as_str).collect(),
        ConnectorsCommand::Rename { to, .. } => vec![to.as_str()],
        ConnectorsCommand::Disconnect { alias, .. } => alias.iter().map(String::as_str).collect(),
        _ => Vec::new(),
    }
}

/// The CLI error of a gateway failure.
pub(crate) fn gateway_error(err: &GatewayError) -> Error {
    match err {
        GatewayError::NoToken => Error::new(
            &codes::CONNECTORS_SIGNIN_REQUIRED,
            "connectors need a Nous sign-in: run prometheus auth login nous",
        ),
        other => Error::new(&codes::CONNECTORS_GATEWAY, other.to_string()),
    }
}

async fn list(ctx: &Context<'_>, gateway: &Gateway) -> Result<u8, Error> {
    let items = gateway.list().await.map_err(|e| gateway_error(&e))?;
    if ctx.json {
        println!("{}", json!({"connectors": items}));
        return Ok(0);
    }
    if items.is_empty() {
        println!("no connectors are available for this account");
    }
    for item in &items {
        let state = if item.connected {
            "connected"
        } else {
            "not connected"
        };
        println!("{}  {state}", item.connector);
    }
    Ok(0)
}

async fn accounts(
    ctx: &Context<'_>,
    gateway: &Gateway,
    app: Option<&str>,
    all: bool,
) -> Result<u8, Error> {
    let accounts = gateway.accounts().await.map_err(|e| gateway_error(&e))?;
    let shown: Vec<&Account> = accounts
        .iter()
        .filter(|a| app.is_none_or(|app| a.connector == app))
        .filter(|a| all || !a.disabled)
        .collect();
    if ctx.json {
        let rows: Vec<serde_json::Value> = shown
            .iter()
            .map(|a| {
                let mut row = account_row(a);
                row["connector"] = json!(a.connector);
                row
            })
            .collect();
        println!("{}", json!({"accounts": rows}));
        return Ok(0);
    }
    if shown.is_empty() {
        println!("no connector accounts");
    }
    for account in shown {
        let row = account_row(account);
        println!(
            "{}  {}  {}  {}",
            account.connector,
            account.alias.as_deref().unwrap_or("-"),
            account.label,
            row["status"].as_str().unwrap_or_default()
        );
    }
    Ok(0)
}

/// The non-retired accounts of `app` answering to `name` (every one when
/// `name` is `None`).
fn matching<'a>(accounts: &'a [Account], app: &str, name: Option<&str>) -> Vec<&'a Account> {
    accounts
        .iter()
        .filter(|a| a.connector == app && !a.disabled)
        .filter(|a| name.is_none_or(|n| a.answers_to(n)))
        .collect()
}

/// Exactly one account of `app` named `name`, or the error listing them.
fn one<'a>(accounts: &'a [Account], app: &str, name: Option<&str>) -> Result<&'a Account, Error> {
    let found = matching(accounts, app, name);
    match found.as_slice() {
        [account] => Ok(account),
        [] => Err(Error::new(
            &codes::CONNECTORS_NOT_FOUND,
            match name {
                Some(name) => format!("no {app} account named '{name}'"),
                None => format!("no {app} account"),
            },
        )
        .with_hint("see the accounts with: prometheus connectors accounts")),
        several => {
            let names: Vec<&str> = several.iter().map(|a| a.display_name()).collect();
            Err(Error::new(
                &codes::CLI_USAGE,
                format!("{app} has several accounts: {}", names.join(", ")),
            )
            .with_hint("name one with --alias"))
        }
    }
}

async fn rename(
    ctx: &Context<'_>,
    gateway: &Gateway,
    app: &str,
    account: &str,
    to: &str,
) -> Result<u8, Error> {
    let accounts = gateway.accounts().await.map_err(|e| gateway_error(&e))?;
    let target = one(&accounts, app, Some(account))?;
    match gateway.rename(&target.connection_id, to).await {
        Ok(()) => {}
        Err(GatewayError::Refused {
            status: 404 | 405 | 501,
            ..
        }) => {
            return Err(Error::new(
                &codes::CONNECTORS_GATEWAY,
                "this Nous portal cannot rename connector accounts yet",
            ));
        }
        Err(err) => return Err(gateway_error(&err)),
    }
    if ctx.json {
        println!(
            "{}",
            json!({"connector": app, "renamed": account, "to": to})
        );
    } else {
        println!("renamed {app} account '{account}' to '{to}'");
    }
    Ok(0)
}

async fn disconnect(
    ctx: &Context<'_>,
    gateway: &Gateway,
    app: &str,
    alias: Option<&str>,
    yes: bool,
) -> Result<u8, Error> {
    let accounts = gateway.accounts().await.map_err(|e| gateway_error(&e))?;
    let target = one(&accounts, app, alias)?;
    let name = target.display_name().to_owned();
    if !yes {
        if !crate::prompt::stdin_is_tty() {
            let err = Error::new(
                &codes::CONNECTORS_CONFIRMATION,
                format!("disconnecting {app} account '{name}' needs confirmation"),
            )
            .with_hint("run it on a terminal, or pass --yes");
            crate::output::print_error(&err, ctx.json);
            return Ok(pmt_core::exit::APPROVAL_REQUIRED);
        }
        if !crate::prompt::confirm(&format!("Disconnect {app} account '{name}'?"))? {
            eprintln!("nothing removed");
            return Ok(1);
        }
    }
    gateway
        .remove(&target.connection_id)
        .await
        .map_err(|e| gateway_error(&e))?;
    if ctx.json {
        println!("{}", json!({"connector": app, "removed": name}));
    } else {
        println!("disconnected {app} account '{name}'");
    }
    Ok(0)
}

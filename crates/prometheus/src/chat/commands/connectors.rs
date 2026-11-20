//! `/connectors` (spec: connectors, Connectors slash command): the
//! profile's connectors with their accounts, aliases and states (retired
//! accounts folded into a count), each account with the actions it offers
//! as the commands that run them.
//!
//! `connect`, `reconnect` and `rename` run on the line interface, the TUI
//! and the desktop; `disconnect` only on the TUI and the desktop, and only
//! once confirmed: without `--yes` it answers with the confirming command
//! and removes nothing.

use std::fmt::Write as _;

use pmt_core::error::{Error, codes};
use pmt_i18n::{Strings, t};
use pmt_tools::connectors::Gateway;
use pmt_tools::connectors::names::is_alias;
use pmt_tools::connectors::wire::{Account, ConnectRequest};

use super::Ctx;
use crate::connectors_cmd::gateway_error;
use crate::slash::Surface;

const USAGE: &str = "usage: /connectors [list | connect <app> [--alias <name>] | reconnect <app> [<name>] | rename <app> <name> --to <new> | disconnect <app> [<name>] [--yes]]";

/// Where `disconnect` is offered.
const DISCONNECT_SURFACES: &[Surface] = &[Surface::Tui, Surface::Desktop];

/// The words of a `/connectors` line, with its flags taken out.
#[derive(Debug, Default, PartialEq, Eq)]
struct Line<'a> {
    words: Vec<&'a str>,
    alias: Option<&'a str>,
    to: Option<&'a str>,
    yes: bool,
}

fn parse(args: &str) -> Result<Line<'_>, Error> {
    let mut line = Line::default();
    let mut words = args.split_whitespace();
    while let Some(word) = words.next() {
        match word {
            "--yes" => line.yes = true,
            "--alias" => line.alias = Some(words.next().ok_or_else(usage)?),
            "--to" => line.to = Some(words.next().ok_or_else(usage)?),
            _ => line.words.push(word),
        }
    }
    for name in line.alias.iter().chain(&line.to) {
        if !is_alias(name) {
            return Err(Error::new(
                &codes::CONNECTORS_INVALID_ALIAS,
                format!("invalid account name '{name}'"),
            ));
        }
    }
    Ok(line)
}

fn usage() -> Error {
    Error::new(&codes::CLI_USAGE, USAGE)
}

/// Runs `/connectors`.
pub(super) async fn connectors(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let line = parse(args)?;
    let gateway = crate::connectors_host::gateway_or_error(ctx.scope)?;
    let strings = ctx.strings();
    match line.words.as_slice() {
        [] | ["list"] => list(&gateway, &strings, ctx.surface).await,
        ["connect", app] => connect(&gateway, &strings, app, line.alias, None).await,
        ["reconnect", app, rest @ ..] if rest.len() <= 1 => {
            reconnect(&gateway, &strings, app, rest.first().copied()).await
        }
        ["rename", app, name] => {
            let to = line.to.ok_or_else(usage)?;
            rename(&gateway, &strings, app, name, to).await
        }
        ["disconnect", app, rest @ ..] if rest.len() <= 1 => {
            if !DISCONNECT_SURFACES.contains(&ctx.surface) {
                return Ok(t!(strings, "slash.connectors.disconnect_elsewhere"));
            }
            let name = rest.first().copied().or(line.alias);
            disconnect(&gateway, &strings, app, name, line.yes).await
        }
        _ => Err(usage()),
    }
}

/// The actions an account offers on `surface`, as commands.
fn actions(account: &Account, surface: Surface) -> Vec<String> {
    let (app, name) = (&account.connector, account.display_name());
    let mut actions = Vec::new();
    if !account.is_active() {
        actions.push(format!("/connectors reconnect {app} {name}"));
    }
    actions.push(format!("/connectors rename {app} {name} --to <new>"));
    if DISCONNECT_SURFACES.contains(&surface) {
        actions.push(format!("/connectors disconnect {app} {name}"));
    }
    actions
}

/// The account's state as listed (`connected` for an active one).
fn state(account: &Account) -> &str {
    if account.is_active() {
        "connected"
    } else {
        &account.status
    }
}

async fn list(gateway: &Gateway, strings: &Strings, surface: Surface) -> Result<String, Error> {
    let items = gateway.list().await.map_err(|e| gateway_error(&e))?;
    let accounts = gateway.accounts().await.map_err(|e| gateway_error(&e))?;
    if items.is_empty() && accounts.is_empty() {
        return Ok(t!(strings, "slash.connectors.none"));
    }
    let mut apps: Vec<&str> = items.iter().map(|i| i.connector.as_str()).collect();
    for account in &accounts {
        if !apps.contains(&account.connector.as_str()) {
            apps.push(&account.connector);
        }
    }
    let mut text = String::new();
    for app in apps {
        let own: Vec<&Account> = accounts.iter().filter(|a| a.connector == app).collect();
        let retired = own.iter().filter(|a| a.disabled).count();
        let _ = write!(text, "{app}");
        if retired > 0 {
            let _ = write!(
                text,
                " ({})",
                t!(strings, "slash.connectors.retired", count = retired)
            );
        }
        text.push('\n');
        let live: Vec<&&Account> = own.iter().filter(|a| !a.disabled).collect();
        if live.is_empty() {
            let _ = writeln!(text, "  -  [/connectors connect {app}]");
        }
        for account in live {
            let alias = account.alias.as_deref().unwrap_or("-");
            let _ = write!(text, "  {alias}  {}  {}", account.label, state(account));
            for action in actions(account, surface) {
                let _ = write!(text, "  [{action}]");
            }
            text.push('\n');
        }
    }
    Ok(text.trim_end().to_owned())
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

fn one<'a>(accounts: &'a [Account], app: &str, name: Option<&str>) -> Result<&'a Account, Error> {
    match matching(accounts, app, name).as_slice() {
        [account] => Ok(account),
        [] => Err(Error::new(
            &codes::CONNECTORS_NOT_FOUND,
            match name {
                Some(name) => format!("no {app} account named '{name}'"),
                None => format!("no {app} account"),
            },
        )),
        several => {
            let names: Vec<&str> = several.iter().map(|a| a.display_name()).collect();
            Err(Error::new(
                &codes::CLI_USAGE,
                format!("{app} has several accounts: {}", names.join(", ")),
            ))
        }
    }
}

async fn connect(
    gateway: &Gateway,
    strings: &Strings,
    app: &str,
    alias: Option<&str>,
    repair: Option<&Account>,
) -> Result<String, Error> {
    let request = ConnectRequest {
        connectors: vec![app.to_owned()],
        alias: alias.map(str::to_owned),
        reinitiate: repair.is_some(),
        connection_id: repair.map(|a| a.connection_id.clone()),
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
    let name = result.alias.as_deref().or(alias).unwrap_or(app);
    match (result.status.as_str(), &result.connect_url) {
        ("active", _) => Ok(t!(
            strings,
            "slash.connectors.already",
            connector = app,
            name = name
        )),
        ("initiated", Some(url)) => Ok(t!(
            strings,
            "slash.connectors.open",
            connector = app,
            url = url
        )),
        _ => Err(Error::new(
            &codes::CONNECTORS_GATEWAY,
            format!(
                "connecting {app} failed: {}",
                result.status_reason.as_deref().unwrap_or(&result.status)
            ),
        )),
    }
}

async fn reconnect(
    gateway: &Gateway,
    strings: &Strings,
    app: &str,
    name: Option<&str>,
) -> Result<String, Error> {
    let accounts = gateway.accounts().await.map_err(|e| gateway_error(&e))?;
    let target = one(&accounts, app, name)?;
    connect(gateway, strings, app, target.alias.as_deref(), Some(target)).await
}

async fn rename(
    gateway: &Gateway,
    strings: &Strings,
    app: &str,
    name: &str,
    to: &str,
) -> Result<String, Error> {
    let accounts = gateway.accounts().await.map_err(|e| gateway_error(&e))?;
    let target = one(&accounts, app, Some(name))?;
    gateway
        .rename(&target.connection_id, to)
        .await
        .map_err(|e| gateway_error(&e))?;
    Ok(t!(
        strings,
        "slash.connectors.renamed",
        connector = app,
        name = name,
        to = to
    ))
}

async fn disconnect(
    gateway: &Gateway,
    strings: &Strings,
    app: &str,
    name: Option<&str>,
    yes: bool,
) -> Result<String, Error> {
    let accounts = gateway.accounts().await.map_err(|e| gateway_error(&e))?;
    let target = one(&accounts, app, name)?;
    let shown = target.display_name();
    if !yes {
        let command = format!("/connectors disconnect {app} {shown} --yes");
        return Ok(t!(
            strings,
            "slash.connectors.confirm",
            connector = app,
            name = shown,
            command = command
        ));
    }
    gateway
        .remove(&target.connection_id)
        .await
        .map_err(|e| gateway_error(&e))?;
    Ok(t!(
        strings,
        "slash.connectors.disconnected",
        connector = app,
        name = shown
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(alias: &str, status: &str, disabled: bool) -> Account {
        Account {
            connection_id: format!("ca_{alias}"),
            connector: "gmail".into(),
            status: status.into(),
            label: format!("{alias}@example.com"),
            alias: Some(alias.into()),
            disabled,
        }
    }

    #[test]
    fn a_broken_account_offers_reconnect_and_only_tui_and_desktop_offer_disconnect() {
        let work = account("work", "expired", false);
        let home = account("home", "active", false);
        assert_eq!(
            actions(&work, Surface::Tui),
            [
                "/connectors reconnect gmail work",
                "/connectors rename gmail work --to <new>",
                "/connectors disconnect gmail work"
            ]
        );
        assert_eq!(
            actions(&home, Surface::Cli),
            ["/connectors rename gmail home --to <new>"]
        );
    }

    #[test]
    fn flags_are_taken_out_and_names_checked() {
        let line = parse("disconnect gmail work --yes").unwrap();
        assert_eq!(line.words, ["disconnect", "gmail", "work"]);
        assert!(line.yes);
        assert!(parse("rename gmail work --to Bad_Name").is_err());
        assert!(parse("connect gmail --alias").is_err());
    }
}

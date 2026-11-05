//! Hosted connector actions: status, connect, reconnect and rename.

use serde_json::{Value, json};

use super::args::Target;
use super::{Manager, Pending};
use crate::connectors::client::{Gateway, GatewayError};
use crate::connectors::wire::{Account, ConnectRequest};

/// The gateway's word for a portal without the rename route.
const NO_RENAME: &str = "this Nous portal cannot rename connector accounts yet";

/// `status`: each connector (or each target) with its accounts; retired
/// accounts are listed as such.
pub(super) async fn status(gateway: &Gateway, targets: &[Target]) -> Result<Value, GatewayError> {
    let items = gateway.list().await?;
    let accounts = gateway.accounts().await?;
    let wanted = |name: &str| targets.is_empty() || targets.iter().any(|t| t.name == name);
    let connectors: Vec<Value> = items
        .iter()
        .filter(|i| wanted(&i.connector))
        .map(|item| {
            let listed: Vec<Value> = accounts
                .iter()
                .filter(|a| a.connector == item.connector)
                .map(account_row)
                .collect();
            json!({"connector": item.connector, "connected": item.connected, "accounts": listed})
        })
        .collect();
    let unknown: Vec<&str> = targets
        .iter()
        .filter(|t| !t.mcp && items.iter().all(|i| i.connector != t.name))
        .map(|t| t.name.as_str())
        .collect();
    let mut answer = json!({"connectors": connectors});
    if !unknown.is_empty() {
        answer["unknown"] = json!(unknown);
    }
    Ok(answer)
}

/// One account as listings show it.
#[must_use]
pub fn account_row(account: &Account) -> Value {
    json!({
        "name": account.display_name(),
        "alias": account.alias,
        "label": account.label,
        "status": if account.disabled { "retired" } else { account.status.as_str() },
    })
}

/// The authorizations `connect` starts, with the targets already done.
pub(super) async fn connect(
    gateway: &Gateway,
    targets: &[Target],
) -> Result<(Vec<Pending>, Vec<Value>), GatewayError> {
    let mut pending = Vec::new();
    let mut done = Vec::new();
    for target in targets {
        let request = ConnectRequest {
            connectors: vec![target.name.clone()],
            alias: target.alias.clone(),
            ..ConnectRequest::default()
        };
        started(
            gateway,
            &request,
            target.alias.as_ref(),
            &mut pending,
            &mut done,
        )
        .await?;
    }
    Ok((pending, done))
}

/// The repairs `reconnect` starts: only accounts that are not connected,
/// unless `force` (an account switch).
pub(super) async fn reconnect(
    gateway: &Gateway,
    targets: &[Target],
    force: bool,
) -> Result<(Vec<Pending>, Vec<Value>), GatewayError> {
    let accounts = gateway.accounts().await?;
    let mut pending = Vec::new();
    let mut done = Vec::new();
    for target in targets {
        let own: Vec<&Account> = accounts
            .iter()
            .filter(|a| a.connector == target.name && !a.disabled)
            .filter(|a| {
                target
                    .alias
                    .as_ref()
                    .is_none_or(|alias| a.answers_to(alias))
            })
            .collect();
        if own.is_empty() {
            done.push(json!({"connector": target.name, "status": "not_found",
                "message": format!("no {} account to reconnect; use action \"connect\"", target.name)}));
        }
        for account in own {
            if account.is_active() && !force {
                done.push(json!({"connector": target.name, "account": account.display_name(),
                    "status": "connected", "message": "already connected; nothing done (force: true switches the account)"}));
                continue;
            }
            let request = ConnectRequest {
                connectors: vec![target.name.clone()],
                reinitiate: true,
                alias: account.alias.clone(),
                connection_id: Some(account.connection_id.clone()),
            };
            started(
                gateway,
                &request,
                account.alias.as_ref(),
                &mut pending,
                &mut done,
            )
            .await?;
        }
    }
    Ok((pending, done))
}

async fn started(
    gateway: &Gateway,
    request: &ConnectRequest,
    alias: Option<&String>,
    pending: &mut Vec<Pending>,
    done: &mut Vec<Value>,
) -> Result<(), GatewayError> {
    for result in gateway.connect(request).await? {
        let alias = result.alias.clone().or_else(|| alias.cloned());
        match (result.status.as_str(), result.connect_url) {
            ("initiated", Some(url)) => pending.push(Pending {
                connector: result.connector,
                alias,
                connection_id: result.connection_id,
                url,
            }),
            ("active", _) => {
                done.push(
                    json!({"connector": result.connector, "alias": alias, "status": "connected"}),
                );
            }
            _ => done.push(
                json!({"connector": result.connector, "alias": alias, "status": "failed",
                "reason": result.status_reason.unwrap_or_else(|| result.status.clone())}),
            ),
        }
    }
    Ok(())
}

/// `rename`: sets the alias of the account named (alias or label) by the
/// target, or of the connector's only account.
pub(super) async fn rename(gateway: &Gateway, target: &Target) -> Result<Value, String> {
    let to = target.to.as_deref().unwrap_or_default();
    let accounts = gateway.accounts().await.map_err(|e| e.to_string())?;
    let own: Vec<&Account> = accounts
        .iter()
        .filter(|a| a.connector == target.name && !a.disabled)
        .collect();
    let chosen: Vec<&&Account> = match &target.alias {
        Some(name) => own.iter().filter(|a| a.answers_to(name)).collect(),
        None => own.iter().collect(),
    };
    let [account] = chosen.as_slice() else {
        let names: Vec<&str> = own.iter().map(|a| a.display_name()).collect();
        return Err(if names.is_empty() {
            format!("the user has no {} account", target.name)
        } else {
            format!(
                "name the {} account to rename with alias (its name or label): {}",
                target.name,
                names.join(", ")
            )
        });
    };
    match gateway.rename(&account.connection_id, to).await {
        Ok(()) => Ok(
            json!({"connector": target.name, "renamed": account.display_name(), "to": to,
            "message": format!("Tell the user their {} account is now named '{to}'.", target.name)}),
        ),
        Err(GatewayError::Refused {
            status: 404 | 405 | 501,
            ..
        }) => Err(NO_RENAME.to_owned()),
        Err(err) => Err(err.to_string()),
    }
}

impl Manager {
    /// The outcomes of `pending` with `done`: a card on surfaces with one,
    /// else a `connect_url` per target for the user to open.
    pub(super) async fn finish(
        &self,
        gateway: &Gateway,
        card: Option<&dyn super::Card>,
        pending: Vec<Pending>,
        mut done: Vec<Value>,
    ) -> Value {
        let (outcomes, note) = match (card, pending.is_empty()) {
            (_, true) => (Vec::new(), None),
            (Some(card), false) => (
                super::card::wait(card, gateway, &pending, self.timeout, self.poll).await,
                Some(
                    "If the user skipped a connection or did not finish it, do not ask again unless they ask.",
                ),
            ),
            (None, false) => (
                pending
                    .iter()
                    .map(|p| {
                        let mut outcome = p.outcome("not_connected");
                        outcome["connect_url"] = json!(p.url);
                        outcome
                    })
                    .collect(),
                Some(
                    "Give the user each connect_url to open and sign in; never open it yourself. Check with action \"status\" after they say they are done.",
                ),
            ),
        };
        done.extend(outcomes);
        let named: Vec<&str> = pending.iter().filter_map(|p| p.alias.as_deref()).collect();
        let mut answer = json!({"results": done});
        if let Some(note) = note {
            answer["note"] = json!(note);
        }
        if !named.is_empty() {
            answer["tell_user"] =
                json!(format!("the new account is named '{}'", named.join("', '")));
        }
        answer
    }
}

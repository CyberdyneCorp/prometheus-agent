//! `vault.list`, `vault.add`, `vault.remove` and `vault.sources` (spec:
//! security-boundaries, Vault management CLI): the desktop and dashboard
//! settings pages manage the profile's credential vault. Results carry
//! metadata only; secret values arrive in `vault.add` and are never sent
//! back.

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_rpc::contracts as c;
use pmt_rpc::{Catalog, RpcError};
use pmt_vault::{Address, Card, Item, Kind, Login, Meta, Secret, Vault};

use crate::methods::{profile_for, rpc};
use crate::router::Router;
use crate::server::Call;

/// What the host lends the vault methods.
pub struct VaultAccess {
    /// The profile's vault.
    pub vault: Vault,
    /// The profile (its `[vault]` settings and `config.toml`).
    pub scope: ProfileScope,
}

/// Registers the vault methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router
        .register::<c::VaultList, _, _>(catalog, list)?
        .register::<c::VaultAdd, _, _>(catalog, add)?
        .register::<c::VaultRemove, _, _>(catalog, remove)?
        .register::<c::VaultSources, _, _>(catalog, sources)?;
    Ok(())
}

fn access(call: &Call, profile: Option<&str>) -> Result<VaultAccess, RpcError> {
    let profile = profile_for(call, profile)?;
    call.server.host().vault(&profile).map_err(|e| rpc(&e))
}

fn item(meta: Meta) -> c::VaultItem {
    c::VaultItem {
        handle: meta.handle,
        kind: meta.kind.as_str().to_owned(),
        label: meta.label,
        origins: meta.origins,
        identifier: meta.identifier,
        totp: meta.totp,
    }
}

async fn list(call: Call, params: c::VaultListParams) -> Result<c::VaultItems, RpcError> {
    let access = access(&call, params.profile.as_deref())?;
    let items = access.vault.list().map_err(|e| rpc(&e))?;
    Ok(c::VaultItems {
        items: items.into_iter().map(item).collect(),
    })
}

async fn add(call: Call, params: c::VaultAddParams) -> Result<c::VaultItem, RpcError> {
    let access = access(&call, params.profile.as_deref())?;
    let secret = secret(params.login, params.card, params.address).map_err(|e| rpc(&e))?;
    let kind = match secret {
        Secret::Login(_) => Kind::Login,
        Secret::Card(_) => Kind::Card,
        Secret::Address(_) => Kind::Address,
    };
    let storage = access.scope.config().vault.key_storage;
    let meta = access
        .vault
        .add(
            Item {
                handle: kind.new_handle(),
                label: params.label,
                secret,
            },
            storage,
        )
        .map_err(|e| rpc(&e))?;
    Ok(item(meta))
}

/// The one item `vault.add` names.
fn secret(
    login: Option<c::VaultLoginInput>,
    card: Option<c::VaultCardInput>,
    address: Option<c::VaultAddressInput>,
) -> Result<Secret, Error> {
    let invalid = |why: &str| Error::new(&codes::RPC_INVALID_PARAMS, format!("vault.add: {why}"));
    match (login, card, address) {
        (Some(login), None, None) => {
            let origins = login
                .origins
                .iter()
                .map(|o| {
                    pmt_vault::origin::of(o).ok_or_else(|| invalid("an origin is not https://host"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            if origins.is_empty() || login.password.is_empty() {
                return Err(invalid("a login needs origins and a password"));
            }
            Ok(Secret::Login(Login {
                origins,
                identifier: login.identifier,
                password: login.password,
                totp: login.totp.filter(|t| !t.is_empty()),
            }))
        }
        (None, Some(card), None) => Ok(Secret::Card(Card {
            holder: card.holder,
            number: card.number,
            expiry: card.expiry,
            cvc: card.cvc,
        })),
        (None, None, Some(a)) => Ok(Secret::Address(Address {
            name: a.name,
            line1: a.line1,
            line2: a.line2,
            city: a.city,
            region: a.region,
            postal_code: a.postal_code,
            country: a.country,
        })),
        _ => Err(invalid("give exactly one of login, card and address")),
    }
}

async fn remove(call: Call, params: c::VaultRemoveParams) -> Result<c::VaultItem, RpcError> {
    let access = access(&call, params.profile.as_deref())?;
    access
        .vault
        .remove(&params.handle)
        .map(item)
        .map_err(|e| rpc(&e))
}

async fn sources(
    call: Call,
    params: c::VaultSourcesParams,
) -> Result<c::VaultSourceList, RpcError> {
    let access = access(&call, params.profile.as_deref())?;
    let change = params
        .enable
        .as_deref()
        .map(|s| (s, true))
        .or(params.disable.as_deref().map(|s| (s, false)));
    let mut config = access.scope.config().vault.clone();
    if let Some((name, on)) = change {
        let key = match name {
            "onepassword" => "vault.onepassword.enabled",
            "bitwarden" => "vault.bitwarden.enabled",
            other => {
                return Err(rpc(&Error::new(
                    &codes::RPC_INVALID_PARAMS,
                    format!("unknown vault source '{other}' (onepassword, bitwarden)"),
                )));
            }
        };
        access
            .scope
            .profile()
            .config_file()
            .set_all(&[(key, if on { "true" } else { "false" })])
            .map_err(|e| rpc(&e))?;
        match name {
            "onepassword" => config.onepassword.enabled = on,
            _ => config.bitwarden.enabled = on,
        }
    }
    Ok(c::VaultSourceList {
        sources: vec![
            c::VaultSource {
                name: "onepassword".to_owned(),
                enabled: config.onepassword.enabled,
                available: false,
            },
            c::VaultSource {
                name: "bitwarden".to_owned(),
                enabled: config.bitwarden.enabled,
                available: false,
            },
        ],
    })
}

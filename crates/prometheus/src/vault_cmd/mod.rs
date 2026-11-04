//! `prometheus vault` (spec: security-boundaries, Vault management CLI;
//! Credential vault storage): list, add, remove, password-manager sources
//! and the key. Only metadata is ever printed; secret values are read from
//! a masked prompt (or stdin, one per line, when it is not a terminal) and
//! never from the command line.

mod args;

use std::fmt::Write as _;
use std::io::Read as _;
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_config::schema::VaultKeyStorage;
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use pmt_vault::{Address, Card, Item, Kind, Login, Meta, Secret, Vault};
use serde::Serialize;

pub use args::{AddArgs, KeyCommand, KeyTarget, SourceArg, VaultCommand};

use crate::context::Context;
use crate::output::{document, print_raw};
use crate::prompt;

/// Runs one `vault` subcommand.
///
/// # Errors
///
/// `vault.*` errors, `vault.secret_on_argv` (2), config writes.
pub fn run(command: &VaultCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    if let VaultCommand::Add(args) = command
        && let Some(flag) = args.secret_flag()
    {
        return Err(Error::new(
            &codes::VAULT_SECRET_ON_ARGV,
            format!("{flag} is not accepted: secrets are read only from a masked prompt, never from the command line"),
        )
        .with_hint("run prometheus vault add without it; it asks for each secret with the input hidden"));
    }
    let scope = ctx.scope()?;
    let vault = open(&scope);
    match command {
        VaultCommand::List => list(&vault, ctx.json),
        VaultCommand::Add(args) => add(&vault, &scope, args),
        VaultCommand::Rm { handle, yes } => remove(&vault, handle, *yes),
        VaultCommand::Sources { enable, disable } => sources(&scope, *enable, *disable),
        VaultCommand::Key { command } => key(&vault, &scope, command.as_ref()),
    }
}

/// The profile's vault over the OS keychain (used only where the vault's
/// keyref or `vault.key_storage` says so).
#[must_use]
pub fn open(scope: &ProfileScope) -> Vault {
    Vault::of(scope.dir(), Arc::new(pmt_config::secrets::OsKeychain))
}

#[derive(Serialize)]
struct Listing {
    items: Vec<Meta>,
}

fn list(vault: &Vault, json: bool) -> Result<u8, Error> {
    let items = vault.list()?;
    if json {
        return document("vault list", &Listing { items });
    }
    if items.is_empty() {
        return print_raw("the vault is empty; add an item with prometheus vault add\n");
    }
    let mut out = String::new();
    for item in &items {
        let _ = write!(
            out,
            "{}  {}  {}",
            item.handle,
            item.kind.as_str(),
            item.label
        );
        if !item.origins.is_empty() {
            let _ = write!(out, "  {}", item.origins.join(","));
        }
        if let Some(identifier) = &item.identifier {
            let _ = write!(out, "  {identifier}");
        }
        if item.totp {
            out.push_str("  totp");
        }
        out.push('\n');
    }
    print_raw(&out)
}

/// Secret values: masked prompts on a terminal, else stdin lines in order.
struct Secrets {
    piped: Option<std::vec::IntoIter<String>>,
}

impl Secrets {
    fn new() -> Result<Self, Error> {
        if prompt::stdin_is_tty() {
            return Ok(Self { piped: None });
        }
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot read stdin: {e}")))?;
        let lines: Vec<String> = text.lines().map(str::to_owned).collect();
        Ok(Self {
            piped: Some(lines.into_iter()),
        })
    }

    /// The next value; `optional` ones may be empty.
    fn next(&mut self, what: &str, optional: bool) -> Result<Option<SecretString>, Error> {
        let value = if let Some(lines) = &mut self.piped {
            lines.next().unwrap_or_default()
        } else {
            let skip = if optional { ", Enter skips" } else { "" };
            rpassword::prompt_password(format!("{what} (input hidden{skip}): ")).map_err(|e| {
                Error::new(&codes::OUTPUT_IO, format!("cannot read the {what}: {e}"))
            })?
        };
        if value.is_empty() && !optional {
            return Err(Error::new(
                &codes::SECRET_EMPTY,
                format!("no {what} was given"),
            ));
        }
        Ok((!value.is_empty()).then(|| SecretString::new(value)))
    }
}

fn required(value: Option<&String>, flag: &str, kind: &str) -> Result<String, Error> {
    value
        .filter(|v| !v.trim().is_empty())
        .cloned()
        .ok_or_else(|| {
            Error::new(
                &codes::CLI_USAGE,
                format!("vault add --{kind} needs --{flag}"),
            )
        })
}

fn add(vault: &Vault, scope: &ProfileScope, args: &AddArgs) -> Result<u8, Error> {
    let kind = if args.card {
        Kind::Card
    } else if args.address {
        Kind::Address
    } else {
        Kind::Login
    };
    let secret = match kind {
        Kind::Login => Secret::Login(login(args)?),
        Kind::Card => Secret::Card(card(args)?),
        Kind::Address => Secret::Address(address(args)?),
    };
    let label = match (&args.label, &secret) {
        (Some(label), _) => label.clone(),
        (None, Secret::Login(login)) => login.origins.join(", "),
        (None, _) => kind.as_str().to_owned(),
    };
    let item = Item {
        handle: kind.new_handle(),
        label,
        secret,
    };
    let meta = vault.add(item, scope.config().vault.key_storage)?;
    eprintln!(
        "added {} {} ({})",
        meta.kind.as_str(),
        meta.handle,
        meta.label
    );
    Ok(0)
}

fn login(args: &AddArgs) -> Result<Login, Error> {
    let identifier = required(args.identifier.as_ref(), "identifier", "login")?;
    if args.origins.is_empty() {
        return Err(Error::new(
            &codes::CLI_USAGE,
            "vault add --login needs --origin https://host (repeatable)",
        ));
    }
    let origins = args
        .origins
        .iter()
        .map(|o| {
            pmt_vault::origin::of(o).ok_or_else(|| {
                Error::new(
                    &codes::CLI_USAGE,
                    format!("'{o}' is not a web origin (https://host)"),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut secrets = Secrets::new()?;
    let password = secrets.next(&format!("password for {identifier}"), false)?;
    let totp = if args.with_totp {
        let seed = secrets.next("TOTP seed (base32)", true)?;
        if let Some(seed) = &seed
            && pmt_vault::totp::code(seed.expose(), 0).is_none()
        {
            return Err(Error::new(&codes::CLI_USAGE, "the TOTP seed is not base32"));
        }
        seed.map(|s| s.expose().to_owned())
    } else {
        None
    };
    Ok(Login {
        origins,
        identifier,
        password: password.map(|p| p.expose().to_owned()).unwrap_or_default(),
        totp,
    })
}

fn card(args: &AddArgs) -> Result<Card, Error> {
    let holder = required(args.holder.as_ref(), "holder", "card")?;
    let expiry = required(args.expiry.as_ref(), "expiry", "card")?;
    let mut secrets = Secrets::new()?;
    let number = secrets.next("card number", false)?;
    let cvc = secrets.next("security code", false)?;
    let text = |s: Option<SecretString>| s.map(|s| s.expose().to_owned()).unwrap_or_default();
    Ok(Card {
        holder,
        number: text(number),
        expiry,
        cvc: text(cvc),
    })
}

fn address(args: &AddArgs) -> Result<Address, Error> {
    let get = |value: &Option<String>| value.clone().unwrap_or_default();
    Ok(Address {
        name: required(args.name.as_ref(), "name", "address")?,
        line1: required(args.line1.as_ref(), "line1", "address")?,
        line2: get(&args.line2),
        city: required(args.city.as_ref(), "city", "address")?,
        region: get(&args.region),
        postal_code: required(args.postal_code.as_ref(), "postal-code", "address")?,
        country: required(args.country.as_ref(), "country", "address")?,
    })
}

fn remove(vault: &Vault, handle: &str, yes: bool) -> Result<u8, Error> {
    let item = vault.get(handle)?.meta();
    if !yes {
        let what = format!(
            "this deletes {} '{}' from the vault",
            item.kind.as_str(),
            item.label
        );
        match prompt::confirm_typed(&what, handle)? {
            None => {
                eprintln!("error: deleting {handle} needs confirmation; rerun with --yes");
                return Ok(pmt_core::exit::APPROVAL_REQUIRED);
            }
            Some(false) => {
                eprintln!("not deleted");
                return Ok(1);
            }
            Some(true) => {}
        }
    }
    vault.remove(handle)?;
    eprintln!("deleted {handle}");
    Ok(0)
}

fn source_key(source: SourceArg) -> (&'static str, &'static str) {
    match source {
        SourceArg::Onepassword => ("onepassword", "vault.onepassword.enabled"),
        SourceArg::Bitwarden => ("bitwarden", "vault.bitwarden.enabled"),
    }
}

fn sources(
    scope: &ProfileScope,
    enable: Option<SourceArg>,
    disable: Option<SourceArg>,
) -> Result<u8, Error> {
    if let Some((source, on)) = enable.map(|s| (s, true)).or(disable.map(|s| (s, false))) {
        let (name, key) = source_key(source);
        scope
            .profile()
            .config_file()
            .set_all(&[(key, if on { "true" } else { "false" })])?;
        eprintln!("{} {name}", if on { "enabled" } else { "disabled" });
        return Ok(0);
    }
    let config = &scope.config().vault;
    let mut out = String::new();
    for (name, enabled) in [
        ("onepassword", config.onepassword.enabled),
        ("bitwarden", config.bitwarden.enabled),
    ] {
        let state = if enabled { "enabled" } else { "disabled" };
        let _ = writeln!(
            out,
            "{name}  {state}  unavailable: password-manager sources are not part of this build"
        );
    }
    print_raw(&out)
}

fn key(vault: &Vault, scope: &ProfileScope, command: Option<&KeyCommand>) -> Result<u8, Error> {
    if let Some(KeyCommand::Migrate { to }) = command {
        let target = match to {
            KeyTarget::Keychain => VaultKeyStorage::Keychain,
            KeyTarget::File => VaultKeyStorage::File,
        };
        let location = vault.migrate_key(target)?;
        eprintln!("the vault key is now in the {}", location.kind());
        return Ok(0);
    }
    let location = match vault.key_location()? {
        Some(pmt_vault::Location::File(path)) => format!("file {}", path.display()),
        Some(pmt_vault::Location::Keychain(account)) => format!("keychain entry {account}"),
        None => "none yet (the first vault add creates it)".to_owned(),
    };
    let storage = match scope.config().vault.key_storage {
        VaultKeyStorage::File => "file",
        VaultKeyStorage::Keychain => "keychain",
    };
    let reachable = if vault.keychain_reachable() {
        "yes"
    } else {
        "no"
    };
    print_raw(&format!(
        "key: {location}\nvault.key_storage: {storage}\nkeychain reachable: {reachable}\n"
    ))
}

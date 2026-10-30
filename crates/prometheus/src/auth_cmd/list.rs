//! `prometheus auth list` and `prometheus auth status <provider>`: peeks
//! that never refresh, rotate or contact a provider, and never print key
//! or token material.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use pmt_config::secrets::Source;
use pmt_core::error::Error;
use pmt_providers::auth::{DEFAULT_LABEL, Status, Subscription};
use pmt_providers::registry::Provider;
use serde::Serialize;

use super::Profile;
use crate::context::Context;
use crate::output::{document, print_raw};

/// One credential as listings show it.
#[derive(Debug, Clone, Serialize)]
pub(super) struct Row {
    pub provider: String,
    pub label: String,
    /// `api_key` or `subscription`.
    pub kind: &'static str,
    /// `file`, `keychain` or `env` for keys; how a sign-in was obtained.
    pub source: String,
    /// `ok` or `dead` (benches arrive with credential pools).
    pub status: &'static str,
    /// The provider refused the sign-in's plan for this use.
    pub ineligible: bool,
    /// Benched until (credential pools).
    pub cooldown_until: Option<String>,
    /// The secret holding an API key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// A sign-in's access token expiry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// A sign-in's plan label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// The sign-in record (subscriptions only).
    #[serde(skip)]
    pub subscription: Option<Subscription>,
}

/// Every credential of `provider`, in `auth status` order: API keys
/// (`default` first), then sign-ins by label.
pub(super) fn rows_of(profile: &Profile, provider: &Provider) -> Result<Vec<Row>, Error> {
    let sources = key_sources(profile)?;
    let mut rows = key_rows(provider, &sources);
    for sub in profile.credentials.subscriptions()? {
        if sub.provider == provider.id() {
            rows.push(subscription_row(sub));
        }
    }
    Ok(rows)
}

/// Secret name -> where its winning value comes from.
fn key_sources(profile: &Profile) -> Result<BTreeMap<String, &'static str>, Error> {
    Ok(profile
        .scope
        .secrets()
        .list()?
        .into_iter()
        .filter(|row| !row.shadowed)
        .map(|row| {
            let source = match row.source {
                Source::File => "file",
                Source::Keychain => "keychain",
                Source::Env => "env",
            };
            (row.name, source)
        })
        .collect())
}

fn key_rows(provider: &Provider, sources: &BTreeMap<String, &'static str>) -> Vec<Row> {
    let names = provider.secret_names();
    let Some(first) = names.first() else {
        return Vec::new();
    };
    let mut keys: Vec<(String, String)> = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let label = if i == 0 {
            DEFAULT_LABEL.to_owned()
        } else {
            name.to_ascii_lowercase().replace('_', "-")
        };
        keys.push((label, name.clone()));
    }
    let prefix = format!("{first}_");
    for name in sources.keys() {
        if let Some(rest) = name.strip_prefix(&prefix)
            && !names.contains(name)
        {
            keys.push((rest.to_ascii_lowercase().replace('_', "-"), name.clone()));
        }
    }
    keys.into_iter()
        .filter_map(|(label, name)| {
            let source = sources.get(&name)?;
            Some(Row {
                provider: provider.id().to_owned(),
                label,
                kind: "api_key",
                source: (*source).to_owned(),
                status: "ok",
                ineligible: false,
                cooldown_until: None,
                secret: Some(name),
                expires_at: None,
                plan: None,
                subscription: None,
            })
        })
        .collect()
}

fn subscription_row(sub: Subscription) -> Row {
    Row {
        provider: sub.provider.clone(),
        label: sub.label.clone(),
        kind: "subscription",
        source: sub.source.clone(),
        status: match sub.status {
            Status::Ok => "ok",
            Status::Dead => "dead",
        },
        ineligible: sub.ineligible,
        cooldown_until: None,
        secret: None,
        expires_at: sub.expires_at.clone(),
        plan: sub.plan.clone(),
        subscription: Some(sub),
    }
}

fn describe(row: &Row) -> String {
    let mut line = format!(
        "{}  {}  {}  {}  {}",
        row.provider, row.label, row.kind, row.source, row.status
    );
    if row.ineligible {
        line.push_str("  ineligible (plan refused)");
    }
    if let Some(plan) = &row.plan {
        let _ = write!(line, "  plan {plan}");
    }
    if let Some(expires) = &row.expires_at {
        let _ = write!(line, "  expires {expires}");
    }
    line
}

pub(super) fn list(ctx: &Context<'_>) -> Result<u8, Error> {
    #[derive(Serialize)]
    struct Listing<'a> {
        profile: &'a str,
        credentials: Vec<Row>,
    }
    let profile = Profile::open(ctx)?;
    let mut credentials = Vec::new();
    for provider in profile.registry.iter() {
        credentials.extend(rows_of(&profile, provider)?);
    }
    if ctx.json {
        return document(
            "auth list",
            &Listing {
                profile: profile.scope.name().as_str(),
                credentials,
            },
        );
    }
    if credentials.is_empty() {
        return print_raw("no credentials; add one with: prometheus auth add <provider>\n");
    }
    let text: String = credentials.iter().map(|r| describe(r) + "\n").collect();
    print_raw(&text)
}

pub(super) fn status(ctx: &Context<'_>, name: &str) -> Result<u8, Error> {
    #[derive(Serialize)]
    struct StatusView<'a> {
        profile: &'a str,
        provider: &'a str,
        auth: &'static str,
        login_flows: Vec<&'static str>,
        prefer_credential: &'static str,
        plan_notes: &'a [String],
        credentials: Vec<Row>,
    }
    let profile = Profile::open(ctx)?;
    let provider = profile.registry.lookup(name)?;
    let view = StatusView {
        profile: profile.scope.name().as_str(),
        provider: provider.id(),
        auth: provider.auth().as_str(),
        login_flows: provider
            .login_flows()
            .iter()
            .map(|f| match f {
                pmt_config::schema::LoginFlow::Device => "device",
                pmt_config::schema::LoginFlow::Browser => "browser",
            })
            .collect(),
        prefer_credential: match provider.settings().prefer_credential {
            pmt_config::schema::PreferCredential::Subscription => "subscription",
            pmt_config::schema::PreferCredential::ApiKey => "api_key",
        },
        plan_notes: provider.plan_notes(),
        credentials: rows_of(&profile, provider)?,
    };
    if ctx.json {
        return document("auth status", &view);
    }
    let mut text = format!("{} (auth {})\n", view.provider, view.auth);
    if !view.login_flows.is_empty() {
        let _ = writeln!(text, "sign-in flows: {}", view.login_flows.join(", "));
        let _ = writeln!(text, "prefers: {}", view.prefer_credential);
    }
    for note in view.plan_notes {
        let _ = writeln!(text, "note: {note}");
    }
    if view.credentials.is_empty() {
        let _ = writeln!(
            text,
            "no credentials; add one with: prometheus auth add {}",
            view.provider
        );
    }
    for (i, row) in view.credentials.iter().enumerate() {
        let _ = writeln!(text, "{}. {}", i + 1, describe(row));
    }
    print_raw(&text)
}

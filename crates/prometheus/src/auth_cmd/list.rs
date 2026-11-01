//! `prometheus auth list` and `prometheus auth status <provider>`: peeks
//! that never refresh, rotate or contact a provider, and never print key
//! or token material.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use jiff::Timestamp;
use pmt_config::secrets::Source;
use pmt_core::error::Error;
use pmt_providers::auth::{Status, Subscription};
use pmt_providers::pool;
use pmt_providers::registry::Provider;
use pmt_store::credential_state::CredentialState;
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
    /// `file`, `keychain` or `env` for keys; how a sign-in was obtained;
    /// `borrowed` for another CLI's login.
    pub source: String,
    /// `ok`, `benched` or `dead`.
    pub status: &'static str,
    /// The provider refused the sign-in's plan for this use.
    pub ineligible: bool,
    /// Benched until then.
    pub cooldown_until: Option<String>,
    /// Requests the credential served.
    pub request_count: u64,
    /// The secret holding an API key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// A sign-in's access token expiry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// A sign-in's plan label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// The sign-in record (owned subscriptions only).
    #[serde(skip)]
    pub subscription: Option<Subscription>,
}

impl Row {
    /// Applies the credential's request count and bench.
    fn with_state(mut self, id: &str, states: &[CredentialState], now: Timestamp) -> Self {
        let state = states.iter().find(|s| s.credential == id);
        self.request_count = state.map_or(0, |s| s.request_count);
        if let Some(until) = pool::benched_until(states, id, now) {
            if self.status == "ok" {
                self.status = "benched";
            }
            self.cooldown_until = Some(until.to_string());
        }
        self
    }
}

/// Every credential of `provider`, in `auth status` order: API keys
/// (`default` first), then sign-ins by label, then a borrowed login.
pub(super) fn rows_of(profile: &Profile, provider: &Provider) -> Result<Vec<Row>, Error> {
    let sources = key_sources(profile)?;
    let states = match profile.accounts.state() {
        Some(state) => state.states(provider.id())?,
        None => Vec::new(),
    };
    let now = Timestamp::now();
    let mut rows = key_rows(provider, &sources, &states, now);
    for sub in profile.credentials.subscriptions()? {
        if sub.provider == provider.id() {
            let id = pool::subscription_id(&sub.label);
            rows.push(subscription_row(sub).with_state(&id, &states, now));
        }
    }
    if let Some(row) = borrowed_row(profile, provider) {
        let id = pool::borrowed_id(row.label.trim_start_matches("borrowed:"));
        rows.push(row.with_state(&id, &states, now));
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

fn key_rows(
    provider: &Provider,
    sources: &BTreeMap<String, &'static str>,
    states: &[CredentialState],
    now: Timestamp,
) -> Vec<Row> {
    let names: Vec<String> = sources.keys().cloned().collect();
    pool::key_members(provider, &names)
        .into_iter()
        .map(|member| {
            let id = pool::secret_id(&member.secret);
            Row {
                provider: provider.id().to_owned(),
                label: member.label,
                kind: "api_key",
                source: sources[&member.secret].to_owned(),
                status: "ok",
                ineligible: false,
                cooldown_until: None,
                request_count: 0,
                secret: Some(member.secret),
                expires_at: None,
                plan: None,
                subscription: None,
            }
            .with_state(&id, states, now)
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
        request_count: 0,
        secret: None,
        expires_at: sub.expires_at.clone(),
        plan: sub.plan.clone(),
        subscription: Some(sub),
    }
}

/// The other CLI's login `provider` may borrow (only read when
/// `auth.adopt_external_logins` is on).
fn borrowed_row(profile: &Profile, provider: &Provider) -> Option<Row> {
    let login = profile.accounts.borrowed_logins().login(provider.id())?;
    let tokens = match login.read() {
        Ok(tokens) => tokens?,
        Err(err) => {
            tracing::warn!(cli = login.cli(), error = %err, "cannot read the borrowed login");
            return None;
        }
    };
    Some(Row {
        provider: provider.id().to_owned(),
        label: login.label(),
        kind: "subscription",
        source: "borrowed".to_owned(),
        status: "ok",
        ineligible: false,
        cooldown_until: None,
        request_count: 0,
        secret: None,
        expires_at: tokens.expires_at.map(|t| t.to_string()),
        plan: tokens.plan,
        subscription: None,
    })
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
    if let Some(until) = &row.cooldown_until {
        let _ = write!(line, "  benched until {until}");
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

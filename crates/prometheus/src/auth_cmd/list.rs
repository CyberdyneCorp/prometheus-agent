//! `prometheus auth list` and `prometheus auth status <provider>`: peeks
//! that never refresh, rotate or bench a credential, and never print key
//! or token material; `auth status` asks a subscription's usage endpoint
//! for its plan and quota windows (at most once a minute per credential).

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
    mark_minted(profile, provider, &mut rows)?;
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

/// Keys minted by a sign-in (OpenRouter OAuth) show that source, and
/// `dead` once the provider refused them.
fn mark_minted(profile: &Profile, provider: &Provider, rows: &mut [Row]) -> Result<(), Error> {
    let Some(state) = profile.accounts.state() else {
        return Ok(());
    };
    for row in rows {
        let Some(secret) = &row.secret else { continue };
        if let Some((source, dead)) = state.key_origin(provider.id(), &pool::secret_id(secret))? {
            row.source = source;
            if dead {
                row.status = "dead";
            }
        }
    }
    Ok(())
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
                Source::Managed => "managed",
                Source::Command => "command",
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
        /// Per sign-in label: its plan and quota windows (spec:
        /// model-providers, Subscription plan and quota visibility).
        #[serde(skip_serializing_if = "Option::is_none")]
        quota: Option<BTreeMap<String, Option<pmt_providers::quota::Quota>>>,
        /// Tools that bill this subscription when their provider setting
        /// names its vendor.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tools: Vec<pmt_providers::auth::tools::ToolRoute>,
    }
    let profile = Profile::open(ctx)?;
    let provider = profile.registry.lookup(name)?;
    let credentials = rows_of(&profile, provider)?;
    let quota = quota_of(&profile, provider, &credentials);
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
        credentials,
        quota,
        tools: tools_of(&profile, provider),
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
    for route in &view.tools {
        let _ = writeln!(
            text,
            "tool {}: {} ({})",
            route.tool,
            if route.routed {
                "billed here"
            } else {
                "not routed here"
            },
            route.setting
        );
    }
    credential_lines(&mut text, &view.credentials, view.quota.as_ref());
    print_raw(&text)
}

type Quotas = BTreeMap<String, Option<pmt_providers::quota::Quota>>;

/// One numbered line per credential; a sign-in's quota under it.
fn credential_lines(text: &mut String, credentials: &[Row], quota: Option<&Quotas>) {
    for (i, row) in credentials.iter().enumerate() {
        let _ = writeln!(text, "{}. {}", i + 1, describe(row));
        if row.kind != "subscription" {
            continue;
        }
        let line = match quota.and_then(|q| q.get(&row.label)) {
            Some(Some(quota)) => format!("quota: {}", quota.describe()),
            Some(None) => "quota: not fetched yet".to_owned(),
            None => pmt_providers::quota::NOT_REPORTED.to_owned(),
        };
        let _ = writeln!(text, "   {line}");
    }
}

/// The tools whose provider setting may name `provider`'s vendor.
fn tools_of(profile: &Profile, provider: &Provider) -> Vec<pmt_providers::auth::tools::ToolRoute> {
    let vendor = match provider.id() {
        "nous" => "nous",
        "xai" | "xai-oauth" => "xai",
        _ => return Vec::new(),
    };
    pmt_providers::auth::tools::tool_routing(profile.scope.config(), vendor)
}

/// The plan and quota windows of each owned sign-in of `provider`, when
/// it reports them: the stored probe, refreshed (at most once a minute,
/// with the current access token; never refreshing, rotating or
/// benching the credential) when older. `None` when the provider reports
/// no quota.
fn quota_of(
    profile: &Profile,
    provider: &Provider,
    rows: &[Row],
) -> Option<BTreeMap<String, Option<pmt_providers::quota::Quota>>> {
    let tracker = profile.accounts.quota()?;
    let base_url = provider.base_url()?;
    pmt_providers::quota::endpoint(provider.id(), base_url)?;
    // A sign-in token goes only to the provider's own hosts.
    let host = pmt_config::provider_urls::host_of(base_url).unwrap_or_default();
    let bound = provider
        .key_hosts()
        .iter()
        .any(|pattern| pmt_providers::descriptor::host_matches(pattern, &host));
    let mut quotas = BTreeMap::new();
    for row in rows {
        let Some(sub) = &row.subscription else {
            continue;
        };
        let id = pool::subscription_id(&row.label);
        let token = profile
            .credentials
            .access_token(sub)
            .ok()
            .flatten()
            .filter(|_| bound);
        let quota = match &token {
            Some(token) => {
                crate::model_cmd::block_on(tracker.current(pmt_providers::quota::Probe {
                    provider: provider.id(),
                    credential: &id,
                    base_url,
                    token,
                    account_id: sub.account_id.as_deref(),
                }))
                .ok()
                .flatten()
            }
            None => tracker.stored(provider.id(), &id),
        };
        quotas.insert(row.label.clone(), quota);
    }
    Some(quotas)
}

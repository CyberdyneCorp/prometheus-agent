//! The `secrets` component (spec: hermes-migration, `.env` re-sorted
//! into secrets and config; Credential files imported).
//!
//! `.env` credentials go to the profile's secrets store under the same
//! name and its behaviour settings to `config.toml`. Credential files that
//! a Prometheus provider understands (the `auth.json` credential pools and
//! sign-ins, `.anthropic_oauth.json`, `slack_tokens.json`) are stored as
//! secrets with their expiry; no credential file is ever copied.

use std::collections::{BTreeMap, BTreeSet};

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_core::secret::SecretString;
use pmt_providers::auth::credentials::{Credentials, Obtained, check_label};
use pmt_providers::auth::token::TokenSet;
use pmt_providers::registry::ProviderRegistry;
use serde_json::Value;

use crate::component::Component;
use crate::env::{self, Class, Transform};
use crate::plan::{Dest, Source, Step};
use crate::report::{Disposition, Entry};
use crate::source::read_text;
use crate::writes::Writes;

/// Hermes credential files a profile may hold that Prometheus does not
/// import (they are reported, never copied).
const OTHER_CREDENTIAL_FILES: &[&str] = &[
    "auth/google_oauth.json",
    ".op.env",
    "npmrc",
    "google_token.json",
    "google_client_secret.json",
    "google_chat_user_client_secret.json",
    "google_chat_user_token.json",
    "honcho.json",
    "mem0.json",
    "webhook_subscriptions.json",
    "teams_pipeline_store.json",
    ".copilot_jwt.json",
    "feishu_comment_pairing.json",
];

/// A subscription sign-in to save.
struct SignIn {
    provider: String,
    label: String,
    tokens: TokenSet,
    entry: usize,
}

/// The planned secrets component.
pub struct SecretsStep {
    entries: Vec<Entry>,
    writes: Writes,
    sign_ins: Vec<SignIn>,
}

impl Step for SecretsStep {
    fn component(&self) -> Component {
        Component::Secrets
    }

    fn entries(&mut self) -> &mut Vec<Entry> {
        &mut self.entries
    }

    fn predict(&mut self, profile_dir: &std::path::Path, overwrite: bool) {
        self.writes
            .predict(profile_dir, &mut self.entries, overwrite);
    }

    fn apply(&mut self, dest: &Dest<'_>) -> Result<(), Error> {
        self.writes
            .apply(dest.scope, &mut self.entries, dest.overwrite)?;
        let credentials = Credentials::new(dest.scope.secrets().store().clone());
        for s in &self.sign_ins {
            let existing = credentials.subscription(&s.provider, &s.label)?;
            if existing.is_some() && !dest.overwrite {
                let e = &mut self.entries[s.entry];
                e.disposition = Disposition::Conflict;
                e.reason = format!(
                    "a '{}' sign-in labelled '{}' exists; kept (use --overwrite to replace it)",
                    s.provider, s.label
                );
                continue;
            }
            let obtained = Obtained {
                source: "import:hermes".to_owned(),
                ..Obtained::default()
            };
            credentials.save(&s.provider, &s.label, &s.tokens, &obtained)?;
        }
        Ok(())
    }
}

/// Plans the secrets component.
///
/// # Errors
///
/// `migrate.component` when a credential file exists but cannot be read.
pub fn plan(src: &Source<'_>) -> Result<SecretsStep, Error> {
    let mut step = SecretsStep {
        entries: Vec::new(),
        writes: Writes::default(),
        sign_ins: Vec::new(),
    };
    plan_env(src, &mut step);
    let registry = ProviderRegistry::for_profile(&pmt_config::Config::default(), Vec::new())?;
    if let Some(text) = src.profile.read("auth.json")? {
        plan_auth(src, &registry, &text, &mut step);
    }
    if let Some(text) = src.profile.read(".anthropic_oauth.json")? {
        plan_anthropic(&registry, &text, &mut step);
    }
    if let Some(text) = src.profile.read("slack_tokens.json")? {
        plan_slack(src, &text, &mut step);
    }
    plan_strategies(src, &mut step);
    for file in OTHER_CREDENTIAL_FILES {
        if read_text(&src.profile.path(file))?.is_some() {
            step.entries.push(
                Entry::new(Component::Secrets, *file, Disposition::Unsupported)
                    .because("no Prometheus provider or adapter reads it; not copied"),
            );
        }
    }
    Ok(step)
}

fn push(step: &mut SecretsStep, entry: Entry) -> usize {
    step.entries.push(entry);
    step.entries.len() - 1
}

fn plan_env(src: &Source<'_>, step: &mut SecretsStep) {
    let known = |n: &str| src.host.known_secret(n);
    for e in src.env {
        let item = format!(".env:{}", e.name);
        let entry = |d| Entry::new(Component::Secrets, item.clone(), d);
        if e.value.trim().is_empty() {
            push(step, entry(Disposition::Skipped).because("empty value"));
            continue;
        }
        match env::classify(&e.name, &known) {
            Class::Secret => {
                let i = push(step, entry(Disposition::Secret).to(&e.name));
                step.writes.secret(&e.name, &e.value, i);
            }
            Class::Unclassified => {
                let i = push(
                    step,
                    entry(Disposition::Secret)
                        .to(&e.name)
                        .because("unclassified"),
                );
                step.writes.secret(&e.name, &e.value, i);
            }
            Class::Setting(key, transform) => match setting_value(&e.value, transform) {
                Some(value) => {
                    let i = push(
                        step,
                        entry(Disposition::Transformed)
                            .to(key)
                            .because("moved into config.toml"),
                    );
                    step.writes.key(key, value, i);
                }
                None => {
                    push(
                        step,
                        entry(Disposition::Unsupported)
                            .to(key)
                            .because("not a valid value"),
                    );
                }
            },
            Class::Unsupported => {
                push(
                    step,
                    entry(Disposition::Unsupported).because("no Prometheus setting"),
                );
            }
            Class::ProcessControl => {
                push(
                    step,
                    entry(Disposition::Skipped).because("steers the Hermes process"),
                );
            }
            Class::Platform => {}
        }
    }
}

fn setting_value(text: &str, transform: Transform) -> Option<toml_edit::Value> {
    let text = text.trim();
    match transform {
        Transform::Text => Some(toml_edit::Value::from(text)),
        Transform::Integer => text.parse::<i64>().ok().map(toml_edit::Value::from),
        Transform::BusyMode => ["queue", "steer", "interrupt"]
            .contains(&text)
            .then(|| toml_edit::Value::from(text)),
    }
}

/// The values each secret name will hold after this component (from
/// `.env`), so pooled keys go to the next free numbered sibling.
fn planned_values(step: &SecretsStep) -> BTreeMap<String, String> {
    step.writes
        .secrets
        .iter()
        .map(|s| (s.name.clone(), s.value.expose().to_owned()))
        .collect()
}

fn plan_auth(src: &Source<'_>, registry: &ProviderRegistry, text: &str, step: &mut SecretsStep) {
    let Ok(auth) = serde_json::from_str::<Value>(text) else {
        push(
            step,
            Entry::new(Component::Secrets, "auth.json", Disposition::Failed)
                .because("not valid JSON"),
        );
        return;
    };
    if let Some(pools) = auth.get("credential_pool").and_then(Value::as_object) {
        for (provider, members) in pools {
            let members = members.as_array().map(Vec::as_slice).unwrap_or_default();
            plan_pool(src, registry, provider, members, step);
        }
    }
    if let Some(providers) = auth.get("providers").and_then(Value::as_object) {
        for (provider, state) in providers {
            let item = format!("auth.json:providers.{provider}");
            plan_sign_in(registry, &item, provider, "default", state, step);
        }
    }
}

fn plan_pool(
    src: &Source<'_>,
    registry: &ProviderRegistry,
    provider: &str,
    members: &[Value],
    step: &mut SecretsStep,
) {
    let mut members: Vec<&Value> = members.iter().collect();
    members.sort_by_key(|m| m.get("priority").and_then(Value::as_i64).unwrap_or(0));
    let first_name = registry
        .lookup(provider)
        .ok()
        .and_then(|p| p.secret_names().into_iter().next());
    let mut taken = planned_values(step);
    let mut api_keys = 0;
    for (i, member) in members.iter().enumerate() {
        let label = member.get("label").and_then(Value::as_str).unwrap_or("");
        let item = format!("auth.json:credential_pool.{provider}[{i}]");
        let kind = member
            .get("auth_type")
            .and_then(Value::as_str)
            .unwrap_or("api_key");
        if kind == "oauth" {
            let label = sanitize_label(label).unwrap_or_else(|| format!("hermes-{}", i + 1));
            plan_sign_in(registry, &item, provider, &label, member, step);
            continue;
        }
        let Some(name) = &first_name else {
            push(
                step,
                Entry::new(Component::Secrets, item, Disposition::Unsupported)
                    .because(format!("no Prometheus provider '{provider}'")),
            );
            continue;
        };
        let key = member
            .get("access_token")
            .and_then(Value::as_str)
            .unwrap_or("");
        if key.is_empty() {
            push(
                step,
                Entry::new(Component::Secrets, item, Disposition::Skipped).because("no key"),
            );
            continue;
        }
        api_keys += 1;
        if taken.values().any(|v| v == key) {
            push(
                step,
                Entry::new(Component::Secrets, item, Disposition::Skipped)
                    .because("the same key is imported from .env"),
            );
            continue;
        }
        let target = next_free(name, &taken);
        taken.insert(target.clone(), key.to_owned());
        let index = push(
            step,
            Entry::new(Component::Secrets, item, Disposition::Secret)
                .to(&target)
                .because(format!("credential pool of {provider}")),
        );
        step.writes.secret(&target, key, index);
    }
    if api_keys > 1 && first_name.is_some() {
        let strategy = src
            .get(&format!("credential_pool_strategies.{provider}"))
            .and_then(Value::as_str)
            .unwrap_or("fill_first");
        let key = format!("providers.{}.pool_strategy", canonical(registry, provider));
        let index = push(
            step,
            Entry::new(
                Component::Secrets,
                format!("auth.json:credential_pool.{provider}"),
                Disposition::Transformed,
            )
            .to(&key)
            .because(format!("credential pool of {api_keys} keys ({strategy})")),
        );
        step.writes.key(key, strategy, index);
    }
}

fn canonical(registry: &ProviderRegistry, provider: &str) -> String {
    registry.canonical(provider).unwrap_or(provider).to_owned()
}

/// `NAME`, else the first free `NAME_<n>` (n from 2).
fn next_free(name: &str, taken: &BTreeMap<String, String>) -> String {
    if !taken.contains_key(name) {
        return name.to_owned();
    }
    // `taken` has finitely many names, so one of the first len + 2 is free.
    (2..=taken.len() + 2)
        .map(|n| format!("{name}_{n}"))
        .find(|candidate| !taken.contains_key(candidate))
        .unwrap_or_else(|| name.to_owned())
}

fn sanitize_label(label: &str) -> Option<String> {
    let slug: String = label
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(32)
        .collect();
    check_label(&slug).is_ok().then_some(slug)
}

/// An OAuth state (`access_token`/`accessToken`, `refresh_token`,
/// `expires_at`/`expiresAt`, seconds, milliseconds or RFC 3339).
fn tokens_of(state: &Value) -> Option<TokenSet> {
    let field = |names: &[&str]| names.iter().find_map(|n| state.get(*n));
    let text = |names: &[&str]| {
        field(names)
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(|t| SecretString::new(t.to_owned()))
    };
    let access = text(&["access_token", "accessToken"])?;
    let expires_at = field(&["expires_at_ms", "expires_at", "expiresAt"]).and_then(timestamp);
    Some(TokenSet {
        access_token: access,
        refresh_token: text(&["refresh_token", "refreshToken"]),
        expires_at,
        id_token: text(&["id_token", "idToken"]),
    })
}

fn timestamp(value: &Value) -> Option<Timestamp> {
    match value {
        Value::Number(n) => {
            let n = n.as_f64()?;
            // Milliseconds from 2001 on, seconds before.
            #[allow(clippy::cast_possible_truncation)]
            let ms = if n > 1e12 {
                n as i64
            } else {
                (n * 1000.0) as i64
            };
            Timestamp::from_millisecond(ms).ok()
        }
        Value::String(s) => s.parse::<Timestamp>().ok().or_else(|| {
            jiff::civil::DateTime::strptime("%Y-%m-%dT%H:%M:%S%.f", s)
                .ok()
                .and_then(|d| d.to_zoned(jiff::tz::TimeZone::UTC).ok())
                .map(|z| z.timestamp())
        }),
        _ => None,
    }
}

fn plan_sign_in(
    registry: &ProviderRegistry,
    item: &str,
    provider: &str,
    label: &str,
    state: &Value,
    step: &mut SecretsStep,
) {
    let entry = |d| Entry::new(Component::Secrets, item, d);
    let oauth = registry
        .lookup(provider)
        .ok()
        .filter(|p| !p.login_flows().is_empty());
    let Some(provider) = oauth else {
        push(
            step,
            entry(Disposition::Unsupported)
                .because(format!("no Prometheus sign-in for '{provider}'")),
        );
        return;
    };
    let Some(tokens) = tokens_of(state) else {
        push(step, entry(Disposition::Skipped).because("no access token"));
        return;
    };
    let id = provider.id().to_owned();
    let expiry = tokens
        .expires_at
        .map_or_else(|| "no expiry".to_owned(), |t| format!("expires {t}"));
    let index = push(
        step,
        entry(Disposition::Secret)
            .to(format!("sign-in {id}/{label}"))
            .because(expiry),
    );
    step.sign_ins.push(SignIn {
        provider: id,
        label: label.to_owned(),
        tokens,
        entry: index,
    });
}

fn plan_anthropic(registry: &ProviderRegistry, text: &str, step: &mut SecretsStep) {
    let item = ".anthropic_oauth.json";
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        push(
            step,
            Entry::new(Component::Secrets, item, Disposition::Failed).because("not valid JSON"),
        );
        return;
    };
    let state = value.get("claudeAiOauth").unwrap_or(&value);
    plan_sign_in(registry, item, "anthropic", "default", state, step);
}

fn plan_slack(src: &Source<'_>, text: &str, step: &mut SecretsStep) {
    let item = "slack_tokens.json";
    let Ok(Value::Object(teams)) = serde_json::from_str::<Value>(text) else {
        push(
            step,
            Entry::new(Component::Secrets, item, Disposition::Failed).because("not valid JSON"),
        );
        return;
    };
    let mut taken = planned_values(step);
    let mut seen = BTreeSet::new();
    for (team, entry) in &teams {
        let bot = entry.get("token").and_then(Value::as_str).unwrap_or("");
        let item = format!("{item}:{team}");
        if bot.is_empty() || !seen.insert(bot.to_owned()) || taken.values().any(|v| v == bot) {
            push(
                step,
                Entry::new(Component::Secrets, item, Disposition::Skipped).because("no new token"),
            );
            continue;
        }
        let target = next_free("SLACK_BOT_TOKEN", &taken);
        taken.insert(target.clone(), bot.to_owned());
        let index = push(
            step,
            Entry::new(Component::Secrets, item, Disposition::Secret).to(&target),
        );
        step.writes.secret(&target, bot, index);
    }
    let _ = src;
}

fn plan_strategies(src: &Source<'_>, step: &mut SecretsStep) {
    let Some(strategies) = src
        .get("credential_pool_strategies")
        .and_then(Value::as_object)
    else {
        return;
    };
    for (provider, strategy) in strategies {
        let item = format!("credential_pool_strategies.{provider}");
        let key = format!("providers.{provider}.pool_strategy");
        if step.writes.config.iter().any(|k| k.key == key) {
            push(
                step,
                Entry::new(Component::Secrets, item, Disposition::Mapped).to(&key),
            );
            continue;
        }
        let Some(text) = strategy.as_str() else {
            push(
                step,
                Entry::new(Component::Secrets, item, Disposition::Unsupported)
                    .because("not a strategy name"),
            );
            continue;
        };
        if pmt_config::layers::typed_value(&key, &format!("{text:?}")).is_err() {
            push(
                step,
                Entry::new(Component::Secrets, item, Disposition::Unsupported)
                    .because(format!("unknown strategy '{text}'")),
            );
            continue;
        }
        let index = push(
            step,
            Entry::new(Component::Secrets, item, Disposition::Mapped).to(&key),
        );
        step.writes.key(key, text, index);
    }
}

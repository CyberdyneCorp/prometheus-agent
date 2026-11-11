//! The `gateway` component (spec: hermes-migration, Gateway settings and
//! pairing allowlists).
//!
//! Settings of the platforms Prometheus has an adapter for (from
//! `config.yaml` `platforms.<name>` and the `<PLATFORM>_*` `.env`
//! variables) go to `[gateway.platforms.<name>]`: the token stays a secret
//! (`token_secret` names it), allowlists, home channels, reply mode,
//! typing, busy and streaming settings become config. The adapter arrives
//! disabled, like every migrated adapter, so it never answers from both
//! agents. Approved pairings become approved senders of the same profile.
//! Settings of other platforms are reported `unsupported`.

use std::collections::BTreeMap;

use jiff::Timestamp;
use pmt_core::error::Error;
use serde_json::{Map, Value, json};

use crate::component::Component;
use crate::config::{flatten, json_to_toml};
use crate::env::{self, Class};
use crate::host::Registry;
use crate::plan::{Dest, Source, Step};
use crate::report::{Disposition, Entry};
use crate::writes::Writes;

/// `.env` prefixes and their platform, longest first.
const PREFIXES: &[(&str, &str)] = &[
    ("WHATSAPP_CLOUD_", "whatsapp_cloud"),
    ("WECOM_CALLBACK_", "wecom_callback"),
    ("GOOGLE_CHAT_", "google_chat"),
    ("BLUEBUBBLES_", "bluebubbles"),
    ("MATTERMOST_", "mattermost"),
    ("TELEGRAM_", "telegram"),
    ("DINGTALK_", "dingtalk"),
    ("WHATSAPP_", "whatsapp"),
    ("DISCORD_", "discord"),
    ("SIGNAL_", "signal"),
    ("MATRIX_", "matrix"),
    ("WEBHOOK_", "webhook"),
    ("YUANBAO_", "yuanbao"),
    ("SIMPLEX_", "simplex"),
    ("PHOTON_", "photon"),
    ("FEISHU_", "feishu"),
    ("WEIXIN_", "weixin"),
    ("QQBOT_", "qqbot"),
    ("SLACK_", "slack"),
    ("EMAIL_", "email"),
    ("WECOM_", "wecom"),
    ("TEAMS_", "teams"),
    ("LINE_", "line"),
    ("NTFY_", "ntfy"),
    ("BUZZ_", "buzz"),
    ("SMS_", "sms"),
    ("IRC_", "irc"),
    ("QQ_", "qqbot"),
];

/// The platform an `.env` name configures.
fn platform_of(name: &str) -> Option<(&'static str, &str)> {
    PREFIXES
        .iter()
        .find_map(|(prefix, p)| name.strip_prefix(prefix).map(|rest| (*p, rest)))
}

/// One platform's settings, collected from the config and `.env`.
#[derive(Default)]
struct Platform {
    table: Map<String, Value>,
    allow: Vec<String>,
    entries: Vec<usize>,
    used: bool,
}

/// One approved sender.
struct Pairing {
    platform: String,
    user: String,
    name: Option<String>,
    at: Option<f64>,
    entry: usize,
}

/// The planned gateway component.
pub struct GatewayStep {
    entries: Vec<Entry>,
    writes: Writes,
    pairings: Vec<Pairing>,
}

impl Step for GatewayStep {
    fn component(&self) -> Component {
        Component::Gateway
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
        for p in &self.pairings {
            #[allow(clippy::cast_possible_truncation)]
            let at =
                p.at.and_then(|s| Timestamp::from_millisecond((s * 1000.0) as i64).ok())
                    .unwrap_or_else(Timestamp::now);
            let added = dest
                .store
                .adopt_pairing(&p.platform, &p.user, p.name.as_deref(), at)?;
            if !added {
                let e = &mut self.entries[p.entry];
                e.disposition = Disposition::Skipped;
                "already approved".clone_into(&mut e.reason);
            }
        }
        Ok(())
    }
}

/// Plans the gateway component.
///
/// # Errors
///
/// `migrate.component` when a pairing file cannot be read.
pub fn plan(src: &Source<'_>) -> Result<GatewayStep, Error> {
    let mut step = GatewayStep {
        entries: Vec::new(),
        writes: Writes::default(),
        pairings: Vec::new(),
    };
    let mut platforms: BTreeMap<String, Platform> = BTreeMap::new();
    let supported = |p: &str| src.host.known(Registry::Platform, p, None);
    for root in ["platforms", "gateway.platforms"] {
        if let Some(defs) = src.get(root).and_then(Value::as_object) {
            for (name, def) in defs {
                config_platform(
                    src,
                    &format!("{root}.{name}"),
                    name,
                    def,
                    &mut platforms,
                    &mut step,
                );
            }
        }
    }
    for e in src.env {
        env_entry(src, &e.name, &e.value, &mut platforms, &mut step);
    }
    let global = src.env_value("GATEWAY_ALLOWED_USERS").map(csv);
    for (name, mut p) in platforms {
        if !supported(&name) || !p.used {
            continue;
        }
        if let Some(users) = &global {
            p.allow.extend(users.iter().cloned());
        }
        finish(&name, p, &mut step);
    }
    pairings(src, &mut step)?;
    Ok(step)
}

fn push(step: &mut GatewayStep, entry: Entry) -> usize {
    step.entries.push(entry);
    step.entries.len() - 1
}

fn unsupported(name: &str) -> String {
    format!("no Prometheus adapter for '{name}'")
}

fn config_platform(
    src: &Source<'_>,
    root: &str,
    name: &str,
    def: &Value,
    platforms: &mut BTreeMap<String, Platform>,
    step: &mut GatewayStep,
) {
    let supported = src.host.known(Registry::Platform, name, None);
    let mut leaves = Vec::new();
    flatten(def, root, &mut leaves);
    let p = platforms.entry(name.to_owned()).or_default();
    for (key, value) in leaves {
        let field = key.strip_prefix(&format!("{root}.")).unwrap_or(&key);
        let entry = Entry::new(Component::Gateway, key.clone(), Disposition::Mapped);
        if !supported {
            push(
                step,
                Entry {
                    disposition: Disposition::Unsupported,
                    reason: unsupported(name),
                    ..entry
                },
            );
            continue;
        }
        let index = push(step, entry);
        p.entries.push(index);
        p.used = true;
        let mapped = match field {
            "enabled" | "home_channel.platform" => Some(("", None)),
            "token" | "api_key" => token(src, name, value, p, step, index),
            "home_channel.chat_id" | "home_channel.thread_id" | "home_channel.name" => {
                Some(set(p, field, value.clone()))
            }
            "reply_to_mode" | "typing_indicator" => Some(set(p, field, value.clone())),
            "busy_input_mode" => Some(set(p, "busy.input_mode", value.clone())),
            "streaming" | "streaming.enabled" => Some(set(p, "streaming.enabled", value.clone())),
            f if f.starts_with("allowed_users") || f.starts_with("extra.allowed_users") => {
                p.allow.extend(strings(value));
                Some(("allow_users", None))
            }
            _ => None,
        };
        let e = &mut step.entries[index];
        match mapped {
            Some(("", _)) => {
                e.disposition = Disposition::Transformed;
                e.reason = format!(
                    "the adapter is imported disabled; enable it with: prometheus config set gateway.platforms.{name}.enabled true"
                );
            }
            Some((target, None)) => e.target = Some(format!("gateway.platforms.{name}.{target}")),
            Some((target, Some(why))) => {
                e.disposition = Disposition::Transformed;
                e.target = Some(format!("gateway.platforms.{name}.{target}"));
                e.reason = why;
            }
            None => {
                e.disposition = Disposition::Unsupported;
                "no Prometheus setting".clone_into(&mut e.reason);
            }
        }
    }
}

fn set(p: &mut Platform, key: &str, value: Value) -> (&'static str, Option<String>) {
    insert_path(&mut p.table, key, value);
    (static_key(key), None)
}

/// The static name of a settable key (for the entry target).
fn static_key(key: &str) -> &'static str {
    match key {
        "home_channel.chat_id" => "home_channel.chat_id",
        "home_channel.thread_id" => "home_channel.thread_id",
        "home_channel.name" => "home_channel.name",
        "reply_to_mode" => "reply_to_mode",
        "typing_indicator" => "typing_indicator",
        "busy.input_mode" => "busy.input_mode",
        "streaming.enabled" => "streaming.enabled",
        _ => "",
    }
}

fn insert_path(table: &mut Map<String, Value>, key: &str, value: Value) {
    match key.split_once('.') {
        Some((head, rest)) => {
            let child = table
                .entry(head.to_owned())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(inner) = child {
                insert_path(inner, rest, value);
            }
        }
        None => {
            table.insert(key.to_owned(), value);
        }
    }
}

/// A token inlined in `config.yaml`: stored as `<PLATFORM>_BOT_TOKEN`
/// unless the `.env` holds one.
fn token(
    src: &Source<'_>,
    name: &str,
    value: &Value,
    p: &mut Platform,
    step: &mut GatewayStep,
    index: usize,
) -> Option<(&'static str, Option<String>)> {
    let text = value.as_str().filter(|t| !t.is_empty())?;
    let secret = format!("{}_BOT_TOKEN", name.to_ascii_uppercase());
    if let Some(refd) = crate::config::refs::whole(text) {
        p.table.insert("token_secret".to_owned(), json!(refd));
        return Some(("token_secret", Some(format!("reference to secret {refd}"))));
    }
    if src.env_value(&secret).is_none() {
        step.writes.secret(&secret, text, index);
    }
    p.table.insert("token_secret".to_owned(), json!(secret));
    Some((
        "token_secret",
        Some(format!("token stored as secret {secret}")),
    ))
}

fn csv(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn strings(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items
            .iter()
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
            .collect(),
        Value::String(s) => csv(s),
        Value::Number(n) => vec![n.to_string()],
        _ => Vec::new(),
    }
}

/// One `.env` entry of a platform (credentials are the secrets
/// component's; the gateway names them).
fn env_entry(
    src: &Source<'_>,
    name: &str,
    value: &str,
    platforms: &mut BTreeMap<String, Platform>,
    step: &mut GatewayStep,
) {
    let item = format!(".env:{name}");
    let class = env::classify(name, &|n| src.host.known_secret(n));
    if name == "GATEWAY_ALLOWED_USERS" {
        push(
            step,
            Entry::new(Component::Gateway, item, Disposition::Transformed)
                .because("added to the allowlist of every imported platform"),
        );
        return;
    }
    let Some((platform, rest)) = platform_of(name) else {
        if class == Class::Platform {
            push(
                step,
                Entry::new(Component::Gateway, item, Disposition::Unsupported)
                    .because("no Prometheus setting"),
            );
        }
        return;
    };
    let supported = src.host.known(Registry::Platform, platform, None);
    if class == Class::Secret {
        if supported {
            name_credential(
                platforms.entry(platform.to_owned()).or_default(),
                name,
                rest,
            );
        }
        return;
    }
    if class != Class::Platform {
        return;
    }
    if !supported {
        push(
            step,
            Entry::new(Component::Gateway, item, Disposition::Unsupported)
                .because(unsupported(platform)),
        );
        return;
    }
    let p = platforms.entry(platform.to_owned()).or_default();
    let (disposition, to, why) = env_setting(platform, rest, value, p);
    if disposition != Disposition::Unsupported {
        p.used = true;
    }
    let mut e = Entry::new(Component::Gateway, item, disposition).because(why);
    e.target = to;
    let index = push(step, e);
    p.entries.push(index);
}

/// A platform credential the secrets component stores: the platform is
/// in use, and a token names the adapter's `token_secret`.
fn name_credential(p: &mut Platform, name: &str, rest: &str) {
    if !(rest.ends_with("TOKEN") || rest.ends_with("API_KEY") || rest == "PASSWORD") {
        return;
    }
    p.used = true;
    if !p.table.contains_key("token_secret") && rest.contains("TOKEN") {
        p.table.insert("token_secret".to_owned(), json!(name));
    }
}

/// One platform `.env` setting into `p`: its disposition, target key
/// and reason.
fn env_setting(
    platform: &str,
    rest: &str,
    value: &str,
    p: &mut Platform,
) -> (Disposition, Option<String>, String) {
    let target = |k: &str| Some(format!("gateway.platforms.{platform}.{k}"));
    let done = |k: &str| (Disposition::Transformed, target(k), String::new());
    match rest {
        "ALLOWED_USERS" => {
            p.allow.extend(csv(value));
            done("allow_users")
        }
        "ALLOW_ALL_USERS" if is_true(value) => {
            p.table.insert("allow_all".to_owned(), json!(true));
            done("allow_all")
        }
        "ALLOW_ALL_USERS" => (Disposition::DefaultOmitted, None, String::new()),
        "HOME_CHANNEL" => {
            insert_path(&mut p.table, "home_channel.chat_id", json!(value));
            done("home_channel.chat_id")
        }
        "HOME_CHANNEL_NAME" => {
            insert_path(&mut p.table, "home_channel.name", json!(value));
            done("home_channel.name")
        }
        "REPLY_TO_MODE" => {
            p.table
                .insert("reply_to_mode".to_owned(), json!(value.to_lowercase()));
            done("reply_to_mode")
        }
        "ENABLED" => (
            Disposition::Transformed,
            None,
            format!(
                "the adapter is imported disabled; enable it with: prometheus config set gateway.platforms.{platform}.enabled true"
            ),
        ),
        _ => (
            Disposition::Unsupported,
            None,
            "no Prometheus setting".to_owned(),
        ),
    }
}

fn is_true(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Writes `[gateway.platforms.<name>]` (disabled, allowlist without
/// wildcards: a `*` becomes `allow_all`).
fn finish(name: &str, mut p: Platform, step: &mut GatewayStep) {
    let mut allow: Vec<String> = Vec::new();
    for user in p.allow.drain(..) {
        if user == "*" {
            p.table.insert("allow_all".to_owned(), json!(true));
        } else if !allow.contains(&user) {
            allow.push(user);
        }
    }
    if !allow.is_empty() {
        p.table.insert("allow_users".to_owned(), json!(allow));
    }
    p.table.insert("enabled".to_owned(), json!(false));
    let index = p.entries.first().copied().unwrap_or_else(|| {
        push(
            step,
            Entry::new(
                Component::Gateway,
                format!("platform:{name}"),
                Disposition::Transformed,
            ),
        )
    });
    let schema_ok = serde_json::from_value::<pmt_config::schema::PlatformConfig>(Value::Object(
        p.table.clone(),
    ));
    match (schema_ok, json_to_toml(&Value::Object(p.table))) {
        (Ok(_), Some(value)) => step
            .writes
            .key(format!("gateway.platforms.{name}"), value, index),
        (Err(e), _) => {
            for i in &p.entries {
                let entry = &mut step.entries[*i];
                entry.disposition = Disposition::Failed;
                entry.reason = format!("Prometheus refuses the {name} settings: {e}");
            }
        }
        (_, None) => {}
    }
}

/// Approved senders from `platforms/pairing/<platform>-approved.json`
/// (or the legacy `pairing/` directory when it has content).
fn pairings(src: &Source<'_>, step: &mut GatewayStep) -> Result<(), Error> {
    let legacy = src.profile.path("pairing");
    let has_content = std::fs::read_dir(&legacy).is_ok_and(|mut d| d.next().is_some());
    let dir = if has_content {
        legacy
    } else {
        src.profile.path("platforms/pairing")
    };
    let Ok(listing) = std::fs::read_dir(&dir) else {
        return Ok(());
    };
    let mut files: Vec<String> = listing
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    for file in files {
        if let Some(platform) = file.strip_suffix("-approved.json") {
            approved(src, &dir.join(&file), platform, step)?;
        } else if file.ends_with("-pending.json") {
            push(
                step,
                Entry::new(
                    Component::Gateway,
                    format!("pairing/{file}"),
                    Disposition::Skipped,
                )
                .because("pending requests are not carried; the sender asks again"),
            );
        }
    }
    Ok(())
}

fn approved(
    src: &Source<'_>,
    path: &std::path::Path,
    platform: &str,
    step: &mut GatewayStep,
) -> Result<(), Error> {
    let text = crate::source::read_text(path)?.unwrap_or_default();
    let users = serde_json::from_str::<Value>(&text).ok();
    let Some(users) = users.as_ref().and_then(Value::as_object) else {
        push(
            step,
            Entry::new(
                Component::Gateway,
                format!("pairing/{platform}"),
                Disposition::Failed,
            )
            .because("not valid JSON"),
        );
        return Ok(());
    };
    let supported = src.host.known(Registry::Platform, platform, None);
    for (user, info) in users {
        let item = format!("pairing:{platform}:{user}");
        if !supported {
            push(
                step,
                Entry::new(Component::Gateway, item, Disposition::Unsupported)
                    .because(unsupported(platform)),
            );
            continue;
        }
        let index = push(
            step,
            Entry::new(Component::Gateway, item, Disposition::Mapped)
                .to(format!("approved {platform} sender")),
        );
        step.pairings.push(Pairing {
            platform: platform.to_owned(),
            user: user.clone(),
            name: info
                .get("user_name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .map(str::to_owned),
            at: info.get("approved_at").and_then(Value::as_f64),
            entry: index,
        });
    }
    Ok(())
}

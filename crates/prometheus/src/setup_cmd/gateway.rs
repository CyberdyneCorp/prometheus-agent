//! `prometheus setup gateway [<platform>]` (spec: cli, Setup wizard
//! modes): one messaging adapter, field by field from its config schema.
//! The adapter's credential and every secret-reference option are read
//! hidden and go only to the secrets store (`config.toml` gets the
//! secret's name); the other scalar options show their current value and
//! keep it on Enter; nested tables and lists are left to `prometheus
//! config set`. The adapter is enabled, and when the daemon runs the
//! wizard offers to start it now.

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use serde_json::Value;

use super::{Wizard, plain};
use crate::context::Context;

/// The credential secret every adapter of a platform reads (its
/// `token_secret` default).
fn credentials(platform: &str) -> &'static [&'static str] {
    match platform {
        "telegram" => &[pmt_adapters_telegram::TOKEN_SECRET],
        "discord" => &[pmt_adapters_discord::TOKEN_SECRET],
        "slack" => &[
            pmt_adapters_slack::TOKEN_SECRET,
            pmt_adapters_slack::APP_TOKEN_SECRET,
        ],
        "matrix" => &[pmt_adapters_matrix::TOKEN_SECRET],
        _ => &[],
    }
}

/// What one option field takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Free text.
    Text,
    /// A whole number.
    Integer,
    /// `true` or `false`.
    Bool,
    /// One of these words.
    Choice(Vec<String>),
    /// The name of a secret (its value is asked for hidden).
    Secret,
}

impl Kind {
    /// Whether `answer` is a value of this kind.
    fn accepts(&self, answer: &str) -> bool {
        match self {
            Self::Text | Self::Secret => true,
            Self::Integer => answer.parse::<i64>().is_ok(),
            Self::Bool => matches!(answer, "true" | "false"),
            Self::Choice(words) => words.iter().any(|w| w == answer),
        }
    }
}

/// One scalar option of an adapter's schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Field {
    /// The option's key under `options`.
    pub name: String,
    /// What it takes.
    pub kind: Kind,
    /// Its schema description.
    pub about: String,
    /// Its schema default, as shown.
    pub default: Option<String>,
}

/// The non-null alternative of an optional schema.
fn concrete(schema: &Value) -> &Value {
    for key in ["anyOf", "oneOf"] {
        if let Some(variants) = schema[key].as_array()
            && variants.iter().all(|v| v.get("const").is_none())
            && let Some(first) = variants.iter().find(|v| v["type"] != "null")
        {
            return first;
        }
    }
    schema
}

/// The words of a string enum (`enum`, or `oneOf` of `const`s).
fn choices(schema: &Value) -> Option<Vec<String>> {
    if let Some(words) = schema["enum"].as_array() {
        return Some(
            words
                .iter()
                .filter_map(|w| w.as_str().map(str::to_owned))
                .collect(),
        );
    }
    let variants = schema["oneOf"]
        .as_array()
        .or_else(|| schema["anyOf"].as_array())?;
    variants
        .iter()
        .map(|v| v["const"].as_str().map(str::to_owned))
        .collect()
}

fn kind_of(name: &str, schema: &Value) -> Option<Kind> {
    let schema = concrete(schema);
    if schema["x-secret-ref"] == true || name.ends_with("_secret") {
        return Some(Kind::Secret);
    }
    if let Some(words) = choices(schema) {
        return Some(Kind::Choice(words));
    }
    let types: Vec<&str> = match &schema["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(list) => list.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    match types.iter().find(|t| **t != "null") {
        Some(&"string") => Some(Kind::Text),
        Some(&"integer" | &"number") => Some(Kind::Integer),
        Some(&"boolean") => Some(Kind::Bool),
        _ => None,
    }
}

/// The scalar options of `schema`, by name; tables and lists are not
/// asked for.
pub(crate) fn fields(schema: &Value) -> Vec<Field> {
    let Some(properties) = schema["properties"].as_object() else {
        return Vec::new();
    };
    properties
        .iter()
        .filter_map(|(name, field)| {
            let kind = kind_of(name, field)?;
            let about = field["description"]
                .as_str()
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let default = match &field["default"] {
                Value::Null => None,
                Value::String(text) => Some(text.clone()),
                other => Some(other.to_string()),
            };
            Some(Field {
                name: name.clone(),
                kind,
                about,
                default,
            })
        })
        .collect()
}

/// The secret name a secret-reference option of `platform` is stored
/// under (`TELEGRAM_WEBHOOK_SECRET` for `webhook_secret_secret`).
fn secret_name(platform: &str, field: &str) -> String {
    let base = field.strip_suffix("_secret").unwrap_or(field);
    format!("{platform}_{base}")
        .to_ascii_uppercase()
        .replace('-', "_")
}

/// The platform `setup gateway` works on: the argument, else asked.
fn platform(wizard: &mut Wizard<'_>, given: Option<&str>) -> Result<String, Error> {
    let names: Vec<&str> = pmt_config::platforms::FIRST_PARTY
        .iter()
        .map(|p| p.name)
        .collect();
    let name = match given {
        Some(name) => name.to_owned(),
        None => wizard
            .ask(&format!("platform ({}):", names.join(", ")))?
            .unwrap_or_default(),
    };
    if pmt_config::platforms::options_schema(&name).is_none() {
        return Err(Error::new(
            &codes::CLI_USAGE,
            format!(
                "unknown messaging platform '{name}' (known: {})",
                names.join(", ")
            ),
        ));
    }
    Ok(name)
}

/// The interactive gateway section.
pub(super) fn interactive(
    ctx: &Context<'_>,
    wizard: &mut Wizard<'_>,
    given: Option<&str>,
) -> Result<(), Error> {
    let scope = ctx.scope()?;
    let name = platform(wizard, given)?;
    wizard.say(&format!(
        "Gateway — {name}: every field of its configuration (Enter keeps the value shown)"
    ))?;
    let base = format!("gateway.platforms.{name}");
    for secret in credentials(&name) {
        ask_secret(&scope, wizard, secret, "the adapter's credential")?;
    }
    let mut pairs: Vec<(String, String)> = Vec::new();
    let schema = pmt_config::platforms::options_schema(&name)
        .cloned()
        .unwrap_or_default();
    for field in fields(&schema) {
        let key = format!("{base}.options.{}", field.name);
        if let Some(value) = ask_field(&scope, wizard, &name, &field, &key)? {
            pairs.push((key, value));
        }
    }
    pairs.push((format!("{base}.enabled"), "true".to_owned()));
    let pairs: Vec<(&str, String)> = pairs.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
    if super::write_changed(&scope, &pairs)? {
        wizard.say(&format!("{base} saved"))?;
    }
    offer_start(ctx, wizard, &name)
}

/// Asks for one option; the value to write when it changes.
fn ask_field(
    scope: &ProfileScope,
    wizard: &mut Wizard<'_>,
    platform: &str,
    field: &Field,
    key: &str,
) -> Result<Option<String>, Error> {
    let current = scope.layers().value(key).map(plain);
    if field.kind == Kind::Secret {
        let name = current.unwrap_or_else(|| secret_name(platform, &field.name));
        let stored = ask_secret(scope, wizard, &name, &field.about)?;
        return Ok(stored.then_some(name));
    }
    let hint = match &field.kind {
        Kind::Choice(words) => format!(" ({})", words.join(", ")),
        Kind::Bool => " (true, false)".to_owned(),
        Kind::Integer => " (a number)".to_owned(),
        Kind::Text | Kind::Secret => String::new(),
    };
    let shown = current
        .or_else(|| field.default.clone())
        .unwrap_or_else(|| "unset".to_owned());
    if !field.about.is_empty() {
        wizard.say(&format!("  {}: {}", field.name, field.about))?;
    }
    loop {
        let Some(answer) = wizard.ask(&format!("{}{hint} [{shown}]:", field.name))? else {
            return Ok(None);
        };
        if field.kind.accepts(&answer) {
            return Ok(Some(answer));
        }
        wizard.say(&format!("{} takes{hint}", field.name))?;
    }
}

/// Asks for secret `name` (hidden); stores it only in the secrets store.
/// Whether it is set afterwards.
fn ask_secret(
    scope: &ProfileScope,
    wizard: &mut Wizard<'_>,
    name: &str,
    about: &str,
) -> Result<bool, Error> {
    let present = scope.secrets().resolve(name).is_ok();
    let state = if present { "set" } else { "missing" };
    wizard.say(&format!("  {name} ({about}): {state}"))?;
    match wizard.ask_secret(name)? {
        Some(value) => {
            scope.secrets().store().set(name, &value)?;
            wizard.say(&format!("{name} stored in the secrets store"))?;
            Ok(true)
        }
        None => Ok(present),
    }
}

/// When the daemon runs: offers to start the adapter now.
fn offer_start(ctx: &Context<'_>, wizard: &mut Wizard<'_>, name: &str) -> Result<(), Error> {
    let scope = ctx.scope()?;
    if crate::daemon_client::running(scope.home())?.is_none() {
        return wizard.say(&format!(
            "the {name} adapter starts with the daemon: prometheus daemon start"
        ));
    }
    if wizard.confirm(
        &format!("The daemon runs. Start the {name} adapter now?"),
        true,
    )? {
        crate::daemon_cmd::gateway(&crate::daemon_cmd::GatewayCommand::Reload, ctx)?;
    }
    Ok(())
}

/// `setup gateway --non-interactive`: the `--set` values are written by
/// the caller; the platform's credentials come from the environment into
/// the store, or exit 78.
pub(super) fn non_interactive(ctx: &Context<'_>, platform: Option<&str>) -> Result<(), Error> {
    let Some(name) = platform else {
        return Ok(());
    };
    let scope = ctx.scope()?;
    for secret in credentials(name) {
        if scope.secrets().store().names()?.iter().any(|n| n == secret) {
            continue;
        }
        let value = ctx.launch.secret(secret).ok_or_else(|| {
            Error::new(
                &codes::SETUP_MISSING_VALUE,
                format!("setup needs {secret} and it was not given"),
            )
            .with_hint(format!(
                "export PMT_SECRET_{secret} for this run, or run: prometheus secrets set {secret}"
            ))
        })?;
        scope.secrets().store().set(secret, value)?;
        eprintln!("{secret} stored in the secrets store");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_options_become_scalar_fields() {
        let schema = pmt_config::platforms::options_schema("telegram").unwrap();
        let fields = fields(schema);
        let kind = |name: &str| {
            fields
                .iter()
                .find(|f| f.name == name)
                .map(|f| f.kind.clone())
        };
        assert_eq!(kind("poll_timeout_secs"), Some(Kind::Integer));
        assert_eq!(kind("webhook_secret_secret"), Some(Kind::Secret));
        assert_eq!(kind("slash_commands"), Some(Kind::Bool));
        assert!(
            matches!(kind("mode"), Some(Kind::Choice(words)) if words.contains(&"polling".to_owned()))
        );
        assert_eq!(kind("command_menu"), None, "tables are left to config set");
        assert_eq!(
            secret_name("telegram", "webhook_secret_secret"),
            "TELEGRAM_WEBHOOK_SECRET"
        );
    }
}

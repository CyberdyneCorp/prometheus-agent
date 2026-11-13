//! Typed platform names (spec: platform-adapters, First-party platforms
//! and typed platform names; Default first-party message limits; Platform
//! configuration shape): the first-party adapters this binary ships, the
//! internal `local` and `relay` platforms, and the platforms installed
//! adapter plugins declare. Each platform publishes the schema its
//! `options` table is validated against, and each first-party one its
//! message length maximum (a user may lower it, never raise it).

use std::collections::BTreeMap;
use std::sync::{LazyLock, RwLock};

use schemars::JsonSchema;
use serde_json::Value;

pub use crate::platform_options::*;

/// A first-party adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstParty {
    /// Platform name.
    pub name: &'static str,
    /// How users write it.
    pub display: &'static str,
    /// The longest message the platform accepts, in its own unit.
    pub max_message_length: u32,
    /// The interactive pairing flow its setup form hosts, if any.
    pub pairing: Option<PairingFlow>,
}

/// An interactive pairing flow an adapter declares for its setup form
/// (spec: web-dashboard, Channel setup forms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingFlow {
    /// Link as a `WhatsApp` linked device: refreshed QR codes until the
    /// phone links, at most 120 seconds.
    WhatsappQr,
    /// Create a Telegram bot by scanning a QR code with the onboarding
    /// service (`options.setup_service_url`), at most 180 seconds.
    TelegramCreate,
}

impl PairingFlow {
    /// The name the dashboard uses.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WhatsappQr => "whatsapp_qr",
            Self::TelegramCreate => "telegram_create",
        }
    }
}

/// The first-party adapters and their limits.
pub const FIRST_PARTY: &[FirstParty] = &[
    FirstParty {
        name: "telegram",
        display: "Telegram",
        max_message_length: 4096,
        pairing: Some(PairingFlow::TelegramCreate),
    },
    FirstParty {
        name: "discord",
        display: "Discord",
        max_message_length: 2000,
        pairing: None,
    },
    FirstParty {
        name: "slack",
        display: "Slack",
        max_message_length: 39_000,
        pairing: None,
    },
    FirstParty {
        name: "email",
        display: "Email",
        max_message_length: 50_000,
        pairing: None,
    },
    FirstParty {
        name: "matrix",
        display: "Matrix",
        max_message_length: 32_000,
        pairing: None,
    },
    FirstParty {
        name: "signal",
        display: "Signal",
        max_message_length: 8000,
        pairing: None,
    },
    FirstParty {
        name: "whatsapp",
        display: "WhatsApp",
        max_message_length: 4096,
        pairing: Some(PairingFlow::WhatsappQr),
    },
    FirstParty {
        name: "webhook",
        display: "Webhook",
        max_message_length: 65_536,
        pairing: None,
    },
];

/// Platforms of the gateway itself: attached clients and the relay
/// connector (their limits come from the client or connector).
pub const INTERNAL: &[&str] = &["local", "relay"];

/// The file-backed test platform of debug builds (`PMT_TEST_ADAPTER_DIR`).
pub const TEST: &str = "test";

/// The first-party adapter `name`, if it is one.
#[must_use]
pub fn first_party(name: &str) -> Option<&'static FirstParty> {
    FIRST_PARTY.iter().find(|p| p.name == name)
}

/// Platforms declared by installed, enabled adapter plugins, with their
/// option schemas.
static PLUGINS: LazyLock<RwLock<BTreeMap<String, &'static Value>>> = LazyLock::new(RwLock::default);

/// Declares a platform provided by an installed, enabled adapter plugin
/// (the plugin loader calls it with the schema of the plugin's `options`;
/// `None` accepts no options). Names are case-normalized.
pub fn register_plugin_platform(name: &str, options_schema: Option<Value>) {
    let schema = options_schema.unwrap_or_else(no_options);
    // A plugin's schema lives as long as the process (the validator holds
    // `'static` schemas); plugins are registered once per load.
    let schema: &'static Value = Box::leak(Box::new(schema));
    PLUGINS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(name.to_lowercase(), schema);
}

/// Whether `name` (lower case) is a platform this process knows.
#[must_use]
pub fn is_known(name: &str) -> bool {
    first_party(name).is_some()
        || INTERNAL.contains(&name)
        || (cfg!(debug_assertions) && name == TEST)
        || PLUGINS
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(name)
}

/// The platforms installed adapter plugins declared, in name order.
#[must_use]
pub fn plugin_platforms() -> Vec<String> {
    PLUGINS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .keys()
        .cloned()
        .collect()
}

/// The schema of `name`'s `options` table.
#[must_use]
pub fn options_schema(name: &str) -> Option<&'static Value> {
    match name {
        "telegram" => Some(&TELEGRAM),
        "discord" => Some(&DISCORD),
        "slack" => Some(&SLACK),
        "email" => Some(&EMAIL),
        "matrix" => Some(&MATRIX),
        "signal" => Some(&SIGNAL),
        "whatsapp" => Some(&WHATSAPP),
        "webhook" => Some(&WEBHOOK),
        _ if INTERNAL.contains(&name) || name == TEST => Some(&NONE),
        _ => PLUGINS
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .copied(),
    }
}

fn no_options() -> Value {
    serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false})
}

/// An options schema with nested types inlined: the validator follows
/// `$ref`s into the configuration schema, not into this one.
fn schema_of<T: JsonSchema>() -> Value {
    let generator = schemars::generate::SchemaSettings::draft2020_12()
        .with(|s| s.inline_subschemas = true)
        .into_generator();
    serde_json::to_value(generator.into_root_schema_for::<T>()).expect("the schema serializes")
}

static NONE: LazyLock<Value> = LazyLock::new(no_options);
static TELEGRAM: LazyLock<Value> = LazyLock::new(schema_of::<TelegramOptions>);
static DISCORD: LazyLock<Value> = LazyLock::new(schema_of::<DiscordOptions>);
static SLACK: LazyLock<Value> = LazyLock::new(schema_of::<SlackOptions>);
static EMAIL: LazyLock<Value> = LazyLock::new(schema_of::<EmailOptions>);
static MATRIX: LazyLock<Value> = LazyLock::new(schema_of::<MatrixOptions>);
static SIGNAL: LazyLock<Value> = LazyLock::new(schema_of::<SignalOptions>);
static WHATSAPP: LazyLock<Value> = LazyLock::new(schema_of::<WhatsappOptions>);
static WEBHOOK: LazyLock<Value> = LazyLock::new(schema_of::<WebhookOptions>);

/// Reads a platform's `options` table as its typed options (defaults for
/// missing keys).
///
/// # Errors
///
/// `config.invalid` naming the table when it does not fit (validation
/// normally refused it already).
pub fn options<T: serde::de::DeserializeOwned>(
    platform: &str,
    table: &BTreeMap<String, Value>,
) -> Result<T, pmt_core::Error> {
    let value = Value::Object(table.clone().into_iter().collect());
    serde_json::from_value(value).map_err(|e| {
        pmt_core::Error::new(
            &pmt_core::error::codes::CONFIG_INVALID,
            format!("gateway.platforms.{platform}.options: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_platforms_are_known_once_registered_in_lower_case() {
        assert!(!is_known("mattermost"));
        register_plugin_platform("Mattermost", None);
        assert!(is_known("mattermost"));
        assert!(options_schema("mattermost").is_some());
        assert!(is_known("telegram") && is_known("relay"));
        assert!(!is_known("myspace"));
    }
}

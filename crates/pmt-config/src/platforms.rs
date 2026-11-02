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
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A first-party adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstParty {
    /// Platform name.
    pub name: &'static str,
    /// How users write it.
    pub display: &'static str,
    /// The longest message the platform accepts, in its own unit.
    pub max_message_length: u32,
}

/// The first-party adapters and their limits.
pub const FIRST_PARTY: &[FirstParty] = &[
    FirstParty {
        name: "telegram",
        display: "Telegram",
        max_message_length: 4096,
    },
    FirstParty {
        name: "discord",
        display: "Discord",
        max_message_length: 2000,
    },
    FirstParty {
        name: "slack",
        display: "Slack",
        max_message_length: 39_000,
    },
    FirstParty {
        name: "email",
        display: "Email",
        max_message_length: 50_000,
    },
    FirstParty {
        name: "matrix",
        display: "Matrix",
        max_message_length: 32_000,
    },
    FirstParty {
        name: "signal",
        display: "Signal",
        max_message_length: 8000,
    },
    FirstParty {
        name: "whatsapp",
        display: "WhatsApp",
        max_message_length: 4096,
    },
    FirstParty {
        name: "webhook",
        display: "Webhook",
        max_message_length: 65_536,
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

fn schema_of<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("the schema serializes")
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

/// `gateway.platforms.telegram.options`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TelegramOptions {
    /// Seconds one long-poll `getUpdates` call waits for updates.
    #[schemars(range(min = 1, max = 50))]
    pub poll_timeout_secs: u64,
}

impl Default for TelegramOptions {
    fn default() -> Self {
        Self {
            poll_timeout_secs: 30,
        }
    }
}

/// `gateway.platforms.discord.options`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DiscordOptions {
    /// Answer only messages that mention the bot in server channels
    /// (direct messages are always answered).
    pub require_mention: bool,
}

/// `gateway.platforms.slack.options`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SlackOptions {
    /// The secret holding the Socket Mode app-level token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_token_secret: Option<crate::schema::SecretName>,
}

/// `gateway.platforms.email.options`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EmailOptions {
    /// The mailbox address the agent answers as.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// IMAP server host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imap_host: Option<String>,
    /// IMAP server port.
    pub imap_port: u16,
    /// IMAP transport security. Unset: `tls`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imap_security: Option<MailSecurity>,
    /// The login name, when it is not the address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// SMTP server host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smtp_host: Option<String>,
    /// SMTP server port.
    pub smtp_port: u16,
    /// SMTP transport security. Unset: `tls` on port 465, else
    /// `starttls`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smtp_security: Option<MailSecurity>,
    /// Seconds between two IMAP polls.
    #[schemars(range(min = 1, max = 3600))]
    pub poll_interval_secs: u64,
}

impl Default for EmailOptions {
    fn default() -> Self {
        Self {
            address: None,
            imap_host: None,
            imap_port: 993,
            imap_security: None,
            username: None,
            smtp_host: None,
            smtp_port: 587,
            smtp_security: None,
            poll_interval_secs: 15,
        }
    }
}

/// How a mail connection is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MailSecurity {
    /// TLS from the first byte.
    Tls,
    /// Plain, upgraded with STARTTLS before logging in.
    Starttls,
    /// No encryption (loopback hosts only).
    Plain,
}

impl EmailOptions {
    /// The IMAP security in effect.
    #[must_use]
    pub fn imap_security(&self) -> MailSecurity {
        self.imap_security.unwrap_or(MailSecurity::Tls)
    }

    /// The SMTP security in effect.
    #[must_use]
    pub fn smtp_security(&self) -> MailSecurity {
        self.smtp_security.unwrap_or(if self.smtp_port == 465 {
            MailSecurity::Tls
        } else {
            MailSecurity::Starttls
        })
    }
}

/// `gateway.platforms.matrix.options`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MatrixOptions {
    /// The homeserver URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homeserver: Option<String>,
}

/// `gateway.platforms.signal.options`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SignalOptions {
    /// The signal-cli HTTP daemon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_url: Option<String>,
    /// The account number, in E.164.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

/// `gateway.platforms.whatsapp.options`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WhatsappOptions {
    /// A `WhatsApp` bridge already running (`http://127.0.0.1:<port>`).
    /// Unset: the adapter starts the managed bridge runtime itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bridge_url: Option<String>,
}

/// `gateway.platforms.webhook.options`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookOptions {}

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

//! The Hermes platforms Prometheus serves through catalog adapter plugins
//! (spec: platform-adapters, Catalog adapter set), and how a Hermes
//! profile configures each: one table that the `plugins` component (which
//! plugin to install) and the `gateway` component (what goes into
//! `[gateway.platforms.<id>]`) both read, so they always agree on which
//! platforms the profile used.
//!
//! Hermes turns most of these platforms on from `.env` alone (`hermes
//! gateway setup` writes only `.env`), with variables that do not follow
//! a `<PLATFORM>_` pattern: `enable` mirrors Hermes's own enabling
//! variables (`ENV_ENABLE_CREDENTIALS` and `required_env` of
//! `plugins/platforms/*`, `gateway/config_env.py` for the Graph listener).

use serde_json::{Value, json};

use crate::plan::Source;

/// How a Hermes `.env` value becomes an option value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A string.
    Text,
    /// A comma-separated list.
    List,
    /// An integer.
    Integer,
    /// A boolean (`1`, `true`, `yes`, `on`).
    Flag,
}

/// A Hermes `.env` setting with a plugin option.
pub(crate) struct Opt {
    /// The Hermes variable.
    pub hermes: &'static str,
    /// The option key under `[gateway.platforms.<id>.options]`.
    pub option: &'static str,
    pub kind: Kind,
}

/// A Hermes credential the plugin reads under its own secret name.
pub(crate) struct Secret {
    /// The Hermes variable.
    pub hermes: &'static str,
    /// The plugin's secret (the name it declares and the user grants).
    pub plugin: &'static str,
    /// The option naming that secret, when the plugin takes one.
    pub option: Option<&'static str>,
}

/// One catalog adapter.
pub(crate) struct Adapter {
    /// The platform and plugin id.
    pub id: &'static str,
    /// The `.env` prefixes of its Hermes variables.
    pub prefixes: &'static [&'static str],
    /// Hermes's enabling variables: any of them set turns the platform on
    /// (a flag must be truthy).
    pub enable: &'static [(&'static str, bool)],
    pub options: &'static [Opt],
    pub secrets: &'static [Secret],
}

const fn opt(hermes: &'static str, option: &'static str, kind: Kind) -> Opt {
    Opt {
        hermes,
        option,
        kind,
    }
}

const fn renamed(hermes: &'static str, plugin: &'static str) -> Secret {
    Secret {
        hermes,
        plugin,
        option: None,
    }
}

use Kind::{Flag, Integer, List, Text};

/// Every catalog adapter, in the order of the spec's list.
pub(crate) const ADAPTERS: &[Adapter] = &[
    Adapter {
        id: "whatsapp_cloud",
        prefixes: &["WHATSAPP_CLOUD_"],
        enable: &[
            ("WHATSAPP_CLOUD_PHONE_NUMBER_ID", false),
            ("WHATSAPP_CLOUD_ACCESS_TOKEN", false),
        ],
        options: &[opt(
            "WHATSAPP_CLOUD_PHONE_NUMBER_ID",
            "phone_number_id",
            Text,
        )],
        secrets: &[],
    },
    Adapter {
        id: "msgraph_webhook",
        prefixes: &["MSGRAPH_WEBHOOK_"],
        enable: &[("MSGRAPH_WEBHOOK_ENABLED", true)],
        options: &[
            opt(
                "MSGRAPH_WEBHOOK_ACCEPTED_RESOURCES",
                "accepted_resources",
                List,
            ),
            opt(
                "MSGRAPH_WEBHOOK_ALLOWED_SOURCE_CIDRS",
                "allowed_source_cidrs",
                List,
            ),
        ],
        secrets: &[Secret {
            hermes: "MSGRAPH_WEBHOOK_CLIENT_STATE",
            plugin: "MSGRAPH_CLIENT_STATE",
            option: Some("client_state_secret"),
        }],
    },
    Adapter {
        id: "teams",
        prefixes: &["TEAMS_"],
        enable: &[("TEAMS_CLIENT_ID", false)],
        options: &[
            opt("TEAMS_CLIENT_ID", "client_id", Text),
            opt("TEAMS_TENANT_ID", "tenant_id", Text),
        ],
        secrets: &[],
    },
    Adapter {
        id: "google_chat",
        prefixes: &["GOOGLE_CHAT_"],
        enable: &[("GOOGLE_CHAT_SERVICE_ACCOUNT_JSON", false)],
        options: &[],
        secrets: &[renamed(
            "GOOGLE_CHAT_SERVICE_ACCOUNT_JSON",
            "GOOGLE_CHAT_SERVICE_ACCOUNT_JSON",
        )],
    },
    Adapter {
        id: "mattermost",
        prefixes: &["MATTERMOST_"],
        enable: &[("MATTERMOST_TOKEN", false)],
        options: &[opt("MATTERMOST_URL", "server_url", Text)],
        secrets: &[],
    },
    Adapter {
        id: "dingtalk",
        prefixes: &["DINGTALK_"],
        enable: &[("DINGTALK_CLIENT_ID", false)],
        options: &[opt("DINGTALK_CLIENT_ID", "client_id", Text)],
        secrets: &[],
    },
    Adapter {
        id: "feishu",
        prefixes: &["FEISHU_"],
        enable: &[("FEISHU_APP_ID", false)],
        options: &[
            opt("FEISHU_APP_ID", "app_id", Text),
            opt("FEISHU_CONNECTION_MODE", "connection_mode", Text),
        ],
        secrets: &[],
    },
    Adapter {
        id: "wecom",
        prefixes: &["WECOM_"],
        enable: &[("WECOM_BOT_ID", false)],
        options: &[
            opt("WECOM_BOT_ID", "bot_id", Text),
            opt("WECOM_WEBSOCKET_URL", "websocket_url", Text),
        ],
        secrets: &[renamed("WECOM_SECRET", "WECOM_BOT_SECRET")],
    },
    Adapter {
        id: "wecom_callback",
        prefixes: &["WECOM_CALLBACK_"],
        enable: &[("WECOM_CALLBACK_CORP_ID", false)],
        options: &[],
        // Hermes has one app; it is the plugin's first.
        secrets: &[
            renamed("WECOM_CALLBACK_CORP_SECRET", "WECOM_CALLBACK_CORP_SECRET_1"),
            renamed("WECOM_CALLBACK_TOKEN", "WECOM_CALLBACK_TOKEN_1"),
            renamed(
                "WECOM_CALLBACK_ENCODING_AES_KEY",
                "WECOM_CALLBACK_AES_KEY_1",
            ),
        ],
    },
    Adapter {
        id: "weixin",
        prefixes: &["WEIXIN_"],
        enable: &[("WEIXIN_TOKEN", false), ("WEIXIN_ACCOUNT_ID", false)],
        options: &[opt("WEIXIN_ACCOUNT_ID", "account_id", Text)],
        secrets: &[],
    },
    Adapter {
        id: "bluebubbles",
        prefixes: &["BLUEBUBBLES_"],
        enable: &[("BLUEBUBBLES_SERVER_URL", false)],
        options: &[opt("BLUEBUBBLES_SERVER_URL", "server_url", Text)],
        secrets: &[],
    },
    Adapter {
        id: "photon",
        prefixes: &["PHOTON_"],
        enable: &[("PHOTON_PROJECT_ID", false)],
        options: &[],
        secrets: &[renamed("PHOTON_PROJECT_ID", "PHOTON_PROJECT_ID")],
    },
    Adapter {
        id: "qqbot",
        prefixes: &["QQBOT_", "QQ_"],
        enable: &[("QQ_APP_ID", false)],
        options: &[
            opt("QQ_APP_ID", "app_id", Text),
            opt("QQ_GROUP_ALLOWED_USERS", "group_allow_from", List),
        ],
        secrets: &[renamed("QQ_CLIENT_SECRET", "QQBOT_CLIENT_SECRET")],
    },
    Adapter {
        id: "yuanbao",
        prefixes: &["YUANBAO_"],
        enable: &[("YUANBAO_APP_ID", false), ("YUANBAO_APP_KEY", false)],
        options: &[opt("YUANBAO_APP_ID", "app_id", Text)],
        secrets: &[],
    },
    Adapter {
        id: "line",
        prefixes: &["LINE_"],
        enable: &[("LINE_CHANNEL_ACCESS_TOKEN", false)],
        options: &[],
        secrets: &[],
    },
    Adapter {
        id: "sms",
        prefixes: &["SMS_", "TWILIO_"],
        enable: &[("TWILIO_ACCOUNT_SID", false)],
        options: &[
            opt("TWILIO_ACCOUNT_SID", "account_sid", Text),
            opt("TWILIO_PHONE_NUMBER", "phone_number", Text),
            opt("SMS_WEBHOOK_URL", "public_url", Text),
        ],
        secrets: &[],
    },
    Adapter {
        id: "ntfy",
        prefixes: &["NTFY_"],
        enable: &[("NTFY_TOPIC", false)],
        options: &[
            opt("NTFY_TOPIC", "topic", Text),
            opt("NTFY_SERVER_URL", "server_url", Text),
            opt("NTFY_PUBLISH_TOPIC", "publish_topic", Text),
            opt("NTFY_MARKDOWN", "markdown", Flag),
        ],
        secrets: &[],
    },
    Adapter {
        id: "irc",
        prefixes: &["IRC_"],
        enable: &[("IRC_SERVER", false)],
        options: &[
            opt("IRC_SERVER", "server", Text),
            opt("IRC_PORT", "port", Integer),
            opt("IRC_NICKNAME", "nickname", Text),
            opt("IRC_CHANNEL", "channels", List),
            opt("IRC_USE_TLS", "use_tls", Flag),
        ],
        secrets: &[],
    },
    Adapter {
        id: "raft",
        prefixes: &["RAFT_"],
        enable: &[("RAFT_PROFILE", false)],
        options: &[opt("RAFT_PROFILE", "raft_profile", Text)],
        secrets: &[],
    },
    Adapter {
        id: "buzz",
        prefixes: &["BUZZ_"],
        enable: &[("BUZZ_RELAY_URL", false)],
        options: &[opt("BUZZ_RELAY_URL", "relay_url", Text)],
        secrets: &[],
    },
    Adapter {
        id: "simplex",
        prefixes: &["SIMPLEX_"],
        enable: &[("SIMPLEX_WS_URL", false)],
        options: &[
            opt("SIMPLEX_WS_URL", "ws_url", Text),
            opt("SIMPLEX_AUTO_ACCEPT", "auto_accept", Flag),
        ],
        secrets: &[],
    },
    Adapter {
        id: "homeassistant",
        prefixes: &["HASS_"],
        enable: &[("HASS_TOKEN", false)],
        options: &[opt("HASS_URL", "url", Text)],
        secrets: &[],
    },
];

/// The catalog adapter `id`.
pub(crate) fn adapter(id: &str) -> Option<&'static Adapter> {
    ADAPTERS.iter().find(|a| a.id == id)
}

/// Whether `id` is a catalog adapter.
pub(crate) fn is_adapter(id: &str) -> bool {
    adapter(id).is_some()
}

/// Every `.env` prefix of a catalog adapter, with its platform.
pub(crate) fn prefixes() -> impl Iterator<Item = (&'static str, &'static str)> {
    ADAPTERS
        .iter()
        .flat_map(|a| a.prefixes.iter().map(move |p| (*p, a.id)))
}

/// The catalog platform a `.env` name configures (the longest matching
/// prefix: `WECOM_CALLBACK_` before `WECOM_`).
pub(crate) fn platform_of(name: &str) -> Option<&'static str> {
    prefixes()
        .filter(|(p, _)| name.starts_with(p))
        .max_by_key(|(p, _)| p.len())
        .map(|(_, id)| id)
}

/// Whether the Hermes profile used catalog platform `id`: enabled in
/// `config.yaml`, turned on by one of Hermes's enabling variables, or
/// holding a credential under one of its prefixes.
pub(crate) fn used(src: &Source<'_>, id: &str) -> bool {
    let Some(adapter) = adapter(id) else {
        return false;
    };
    let configured = src
        .get(&format!("platforms.{id}.enabled"))
        .and_then(Value::as_bool)
        == Some(true);
    // Hermes's Home Assistant integration had no enabled flag.
    let present = id == "homeassistant" && src.get("platforms.homeassistant").is_some();
    let enabled = adapter.enable.iter().any(|(name, flag)| {
        src.env_value(name)
            .is_some_and(|v| !v.trim().is_empty() && (!flag || truthy(v)))
    });
    let credential = src.env.iter().any(|e| {
        !e.value.trim().is_empty()
            && crate::env::secret_shaped(&e.name)
            && platform_of(&e.name) == Some(id)
    });
    configured || present || enabled || credential
}

/// A Hermes credential stored under another name for its plugin: the
/// plugin's secret, and its platform.
pub(crate) fn secret(hermes: &str) -> Option<(&'static Adapter, &'static Secret)> {
    ADAPTERS.iter().find_map(|a| {
        a.secrets
            .iter()
            .find(|s| s.hermes == hermes)
            .map(|s| (a, s))
    })
}

/// The option a Hermes setting of platform `id` maps to, with its value.
pub(crate) fn option(id: &str, hermes: &str, value: &str) -> Option<(&'static str, Value)> {
    let opt = adapter(id)?.options.iter().find(|o| o.hermes == hermes)?;
    let value = match opt.kind {
        Kind::Text => json!(value.trim()),
        Kind::List => json!(
            value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        ),
        Kind::Integer => json!(value.trim().parse::<i64>().ok()?),
        Kind::Flag => json!(truthy(value)),
    };
    Some((opt.option, value))
}

/// Hermes's truthy text.
pub(crate) fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_catalog_adapter_has_its_own_prefixes() {
        for a in ADAPTERS {
            assert!(!a.prefixes.is_empty(), "{}", a.id);
            for (name, _) in a.enable {
                assert!(
                    a.prefixes.iter().any(|p| name.starts_with(p)),
                    "{} enables {} outside its prefixes",
                    a.id,
                    name
                );
            }
        }
        assert_eq!(ADAPTERS.len(), 22);
        assert_eq!(
            platform_of("WECOM_CALLBACK_CORP_SECRET"),
            Some("wecom_callback")
        );
        assert_eq!(platform_of("WECOM_SECRET"), Some("wecom"));
        assert_eq!(platform_of("TWILIO_AUTH_TOKEN"), Some("sms"));
    }

    #[test]
    fn options_take_their_kind() {
        assert_eq!(
            option("irc", "IRC_CHANNEL", "#a, #b"),
            Some(("channels", json!(["#a", "#b"])))
        );
        assert_eq!(
            option("irc", "IRC_PORT", "6697"),
            Some(("port", json!(6697)))
        );
        assert_eq!(option("irc", "IRC_PORT", "x"), None);
        assert_eq!(
            option("ntfy", "NTFY_MARKDOWN", "yes"),
            Some(("markdown", json!(true)))
        );
    }
}

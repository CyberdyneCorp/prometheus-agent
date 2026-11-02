//! Delivery targets (spec: messaging-gateway, Home channel and /sethome;
//! agent-interop, One-shot send command; `send_message` tool): the grammar
//! `platform`, `platform:chat_id`, `platform:chat_id:thread_id` and
//! `platform:#channel-name`, its resolution against a profile's
//! configuration (a bare platform is its home channel), and the `MEDIA:`
//! directives of a message body.

use pmt_config::schema::{GatewayConfig, HomeChannel};
use pmt_core::error::{Error, codes};
use pmt_store::gateway::outbox::OutboundAttachment;

/// Where a target points before resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// The platform's home channel.
    Home,
    /// A chat (and thread).
    Chat {
        /// Chat id.
        chat_id: String,
        /// Thread id.
        thread_id: Option<String>,
    },
    /// A channel by name (`#name`).
    Named(String),
}

/// A parsed target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Platform name (lower case).
    pub platform: String,
    /// What it points to.
    pub destination: Destination,
}

/// A resolved destination: one chat of one platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// Platform name.
    pub platform: String,
    /// Chat id.
    pub chat_id: String,
    /// Thread id.
    pub thread_id: Option<String>,
}

impl Resolved {
    /// `platform:chat_id[:thread_id]`.
    #[must_use]
    pub fn label(&self) -> String {
        match &self.thread_id {
            Some(thread) => format!("{}:{}:{thread}", self.platform, self.chat_id),
            None => format!("{}:{}", self.platform, self.chat_id),
        }
    }
}

fn unknown(target: &str, why: &str) -> Error {
    Error::new(
        &codes::GATEWAY_UNKNOWN_TARGET,
        format!("unknown target '{target}': {why}"),
    )
    .with_hint(
        "use platform, platform:chat_id, platform:chat_id:thread_id or platform:#channel-name",
    )
}

impl Target {
    /// Parses `text`.
    ///
    /// # Errors
    ///
    /// `gateway.unknown_target` (exit 66) for a malformed target.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let text = text.trim();
        let mut parts = text.splitn(3, ':');
        let platform = parts.next().unwrap_or_default().to_lowercase();
        if platform.is_empty()
            || !platform
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(unknown(text, "it does not start with a platform name"));
        }
        let destination = match (parts.next(), parts.next()) {
            (None, _) => Destination::Home,
            (Some(chat), None) if chat.starts_with('#') && chat.len() > 1 => {
                Destination::Named(chat[1..].to_owned())
            }
            (Some(chat), thread) if !chat.is_empty() && thread != Some("") => Destination::Chat {
                chat_id: chat.to_owned(),
                thread_id: thread.map(str::to_owned),
            },
            _ => return Err(unknown(text, "the chat id is empty")),
        };
        Ok(Self {
            platform,
            destination,
        })
    }
}

/// The message of a delivery that needs a home channel when none is set.
#[must_use]
pub fn no_home_channel(platform: &str) -> Error {
    Error::new(
        &codes::GATEWAY_NO_HOME_CHANNEL,
        format!("no home channel for {platform}; run /sethome in the target chat"),
    )
}

/// The home channel of `platform` as a destination.
fn home(platform: &str, channel: &HomeChannel) -> Resolved {
    Resolved {
        platform: platform.to_owned(),
        chat_id: channel.chat_id.clone(),
        thread_id: channel.thread_id.clone(),
    }
}

/// Resolves `target` against `gateway` without asking an adapter: a bare
/// platform is its home channel, `#name` the home channel of that name.
/// `Ok(None)` for a `#name` only an adapter could know.
///
/// # Errors
///
/// `gateway.no_home_channel` (66) for a platform without one.
pub fn resolve(gateway: &GatewayConfig, target: &Target) -> Result<Option<Resolved>, Error> {
    let platform = &target.platform;
    let channel = gateway
        .platforms
        .get(platform)
        .and_then(|t| t.home_channel.as_ref());
    match &target.destination {
        Destination::Home => channel
            .map(|c| Some(home(platform, c)))
            .ok_or_else(|| no_home_channel(platform)),
        Destination::Chat { chat_id, thread_id } => Ok(Some(Resolved {
            platform: platform.clone(),
            chat_id: chat_id.clone(),
            thread_id: thread_id.clone(),
        })),
        Destination::Named(name) => Ok(channel
            .filter(|c| c.name.as_deref() == Some(name.as_str()))
            .map(|c| home(platform, c))),
    }
}

/// The error of a `#name` nothing resolved.
#[must_use]
pub fn unknown_channel(platform: &str, name: &str) -> Error {
    unknown(
        &format!("{platform}:#{name}"),
        "no home channel or platform channel has that name",
    )
}

/// Whether the profile enables `platform`.
#[must_use]
pub fn configured(gateway: &GatewayConfig, platform: &str) -> bool {
    gateway.platforms.get(platform).is_some_and(|t| t.enabled)
}

/// The error of a platform the profile does not enable.
#[must_use]
pub fn not_configured(platform: &str) -> Error {
    Error::new(
        &codes::GATEWAY_PLATFORM_NOT_CONFIGURED,
        format!("platform '{platform}' is not configured in this profile"),
    )
    .with_hint(format!(
        "enable it under [gateway.platforms.{platform}] in config.toml"
    ))
}

/// A body's text and attachments: each `MEDIA:<path>` line attaches a
/// file and a `[[as_document]]` line sends every attachment as a document
/// (images uncompressed); both lines leave the text.
#[must_use]
pub fn media(body: &str) -> (String, Vec<OutboundAttachment>) {
    let mut text = Vec::new();
    let mut paths = Vec::new();
    let mut as_document = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(path) = trimmed.strip_prefix("MEDIA:") {
            let path = path.trim();
            if !path.is_empty() {
                paths.push(path.to_owned());
            }
        } else if trimmed == "[[as_document]]" {
            as_document = true;
        } else {
            text.push(line);
        }
    }
    let attachments = paths
        .into_iter()
        .map(|path| OutboundAttachment { path, as_document })
        .collect();
    (text.join("\n").trim().to_owned(), attachments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pmt_config::schema::PlatformConfig;

    #[test]
    fn the_target_grammar() {
        let cases = [
            ("telegram", Destination::Home),
            (
                "Telegram:-1001234567890",
                Destination::Chat {
                    chat_id: "-1001234567890".into(),
                    thread_id: None,
                },
            ),
            (
                "telegram:-100123:7",
                Destination::Chat {
                    chat_id: "-100123".into(),
                    thread_id: Some("7".into()),
                },
            ),
            ("slack:#ops", Destination::Named("ops".into())),
        ];
        for (text, destination) in cases {
            let target = Target::parse(text).unwrap();
            assert_eq!(target.destination, destination, "{text}");
            assert_eq!(
                target.platform,
                text.split(':').next().unwrap().to_lowercase()
            );
        }
        for bad in ["", ":1", "telegram:", "telegram::7", "tele gram:1", "x:1:"] {
            let err = Target::parse(bad).unwrap_err();
            assert_eq!(err.code(), &codes::GATEWAY_UNKNOWN_TARGET, "{bad}");
        }
    }

    #[test]
    fn a_bare_platform_is_its_home_channel() {
        let mut gateway = GatewayConfig::default();
        let err = resolve(&gateway, &Target::parse("slack").unwrap()).unwrap_err();
        assert_eq!(
            err.message(),
            "no home channel for slack; run /sethome in the target chat"
        );
        assert_eq!(err.exit_code(), 66);
        gateway.platforms.insert(
            "telegram".into(),
            PlatformConfig {
                home_channel: Some(HomeChannel {
                    chat_id: "-100123".into(),
                    thread_id: Some("7".into()),
                    name: Some("ops".into()),
                }),
                ..PlatformConfig::default()
            },
        );
        let home = resolve(&gateway, &Target::parse("telegram").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(home.label(), "telegram:-100123:7");
        let named = resolve(&gateway, &Target::parse("telegram:#ops").unwrap()).unwrap();
        assert_eq!(named, Some(home));
        assert_eq!(
            resolve(&gateway, &Target::parse("telegram:#other").unwrap()).unwrap(),
            None
        );
    }

    #[test]
    fn media_directives_become_attachments() {
        let (text, attachments) =
            media("Weekly report\nMEDIA:/data/a.png\n[[as_document]]\nMEDIA: /data/b.pdf");
        assert_eq!(text, "Weekly report");
        assert_eq!(attachments.len(), 2);
        assert!(attachments.iter().all(|a| a.as_document));
        assert_eq!(attachments[1].path, "/data/b.pdf");
        let (text, attachments) = media("RAM 92%");
        assert_eq!(text, "RAM 92%");
        assert!(attachments.is_empty());
    }
}

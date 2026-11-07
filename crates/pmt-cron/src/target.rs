//! Delivery targets (spec: scheduled-jobs, Delivery targets): `local`,
//! `origin`, `none`, `<adapter>` (its home chat), `<adapter>:<chat_id>
//! [:<thread_id>]` and `bot_chat[:<profile>]`.

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

/// Where a job was created from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobOrigin {
    /// A messaging chat.
    Chat {
        /// The platform.
        platform: String,
        /// The chat id.
        chat: String,
        /// The thread id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<String>,
    },
    /// The `api-server` Jobs API (`origin` delivers `local`).
    Api,
}

/// A parsed target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Output stored only.
    Local,
    /// The creating chat.
    Origin,
    /// Nothing beyond the run record.
    None,
    /// An adapter's home chat.
    Home(String),
    /// An explicit chat.
    Chat {
        /// The platform.
        platform: String,
        /// The chat id.
        chat: String,
        /// The thread id.
        thread: Option<String>,
    },
    /// A canonical Bot Chat.
    BotChat(Option<String>),
}

fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// `cron.invalid_job` for a target outside the syntax.
fn unknown(text: &str) -> Error {
    Error::new(
        &codes::CRON_INVALID_JOB,
        format!(
            "unknown delivery target '{text}' (expected local, origin, none, <adapter>, \
             <adapter>:<chat_id>[:<thread_id>] or bot_chat[:<profile>])"
        ),
    )
}

impl Target {
    /// Parses one target.
    ///
    /// # Errors
    ///
    /// `cron.invalid_job` naming the target.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let text = text.trim();
        match text {
            "local" => return Ok(Self::Local),
            "origin" => return Ok(Self::Origin),
            "none" => return Ok(Self::None),
            "bot_chat" => return Ok(Self::BotChat(None)),
            _ => {}
        }
        if let Some(profile) = text.strip_prefix("bot_chat:") {
            return if is_name(profile) {
                Ok(Self::BotChat(Some(profile.to_owned())))
            } else {
                Err(unknown(text))
            };
        }
        let mut parts = text.splitn(3, ':');
        let platform = parts.next().unwrap_or_default();
        if !is_name(platform) {
            return Err(unknown(text));
        }
        match (parts.next(), parts.next()) {
            (None, _) => Ok(Self::Home(platform.to_owned())),
            (Some(chat), thread) if !chat.is_empty() && thread.is_none_or(|t| !t.is_empty()) => {
                Ok(Self::Chat {
                    platform: platform.to_owned(),
                    chat: chat.to_owned(),
                    thread: thread.map(str::to_owned),
                })
            }
            _ => Err(unknown(text)),
        }
    }

    /// The messaging platform it needs, if any.
    #[must_use]
    pub fn platform(&self) -> Option<&str> {
        match self {
            Self::Home(platform) | Self::Chat { platform, .. } => Some(platform),
            _ => None,
        }
    }
}

/// Where one target sends at fire time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// Stored only (`local`, `none`, `origin` without a chat).
    Stored,
    /// A messaging destination as the gateway takes it.
    Messaging(String),
    /// A Bot Chat.
    BotChat(Option<String>),
}

/// Resolves `target` for a job created from `origin`.
#[must_use]
pub fn resolve(target: &Target, origin: Option<&JobOrigin>) -> Resolved {
    match target {
        Target::Local | Target::None => Resolved::Stored,
        Target::Origin => match origin {
            Some(JobOrigin::Chat {
                platform,
                chat,
                thread,
            }) => Resolved::Messaging(chat_target(platform, chat, thread.as_deref())),
            Some(JobOrigin::Api) | None => Resolved::Stored,
        },
        Target::Home(platform) => Resolved::Messaging(platform.clone()),
        Target::Chat {
            platform,
            chat,
            thread,
        } => Resolved::Messaging(chat_target(platform, chat, thread.as_deref())),
        Target::BotChat(profile) => Resolved::BotChat(profile.clone()),
    }
}

fn chat_target(platform: &str, chat: &str, thread: Option<&str>) -> String {
    match thread {
        Some(thread) => format!("{platform}:{chat}:{thread}"),
        None => format!("{platform}:{chat}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{JobOrigin, Resolved, Target, resolve};

    #[test]
    fn targets_parse_and_resolve() {
        assert_eq!(Target::parse("local").unwrap(), Target::Local);
        assert_eq!(
            Target::parse("telegram").unwrap(),
            Target::Home("telegram".into())
        );
        assert_eq!(
            Target::parse("telegram:42:7").unwrap(),
            Target::Chat {
                platform: "telegram".into(),
                chat: "42".into(),
                thread: Some("7".into())
            }
        );
        assert_eq!(
            Target::parse("bot_chat:work").unwrap(),
            Target::BotChat(Some("work".into()))
        );
        for bad in [
            "",
            "Tele gram",
            "telegram:",
            "telegram:1:",
            "bot_chat:",
            ":42",
        ] {
            assert!(Target::parse(bad).is_err(), "{bad}");
        }
        let origin = JobOrigin::Chat {
            platform: "telegram".into(),
            chat: "42".into(),
            thread: Some("7".into()),
        };
        assert_eq!(
            resolve(&Target::Origin, Some(&origin)),
            Resolved::Messaging("telegram:42:7".into())
        );
        assert_eq!(
            resolve(&Target::Origin, Some(&JobOrigin::Api)),
            Resolved::Stored
        );
    }
}

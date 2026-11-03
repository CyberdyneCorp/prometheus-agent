//! Routing identity and deterministic conversation keys (spec:
//! messaging-gateway, Routing identity and deterministic session keys;
//! DM, group and thread isolation).
//!
//! A key is `<profile>:<platform>[@<scope_id>]:<chat_type>:<chat_id>`
//! `[:t:<thread_id>][:u:<participant_id>]`, every id percent-encoding `:`
//! and `%` so a key always parses back into the same fields.

use std::fmt;

/// The kind of a chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatType {
    /// A direct message.
    Dm,
    /// A group chat.
    Group,
    /// A channel.
    Channel,
    /// A thread inside a group or channel.
    Thread,
}

impl ChatType {
    /// The key segment.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dm => "dm",
            Self::Group => "group",
            Self::Channel => "channel",
            Self::Thread => "thread",
        }
    }

    /// Parses a key segment.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "dm" => Self::Dm,
            "group" => Self::Group,
            "channel" => Self::Channel,
            "thread" => Self::Thread,
            _ => return None,
        })
    }
}

/// The immutable identity of one inbound event, resolved before any key
/// is derived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routing {
    /// The adapter instance that received it.
    pub adapter: String,
    /// The owning profile.
    pub profile: String,
    /// Platform name.
    pub platform: String,
    /// Workspace or server id, on platforms that have one.
    pub scope_id: Option<String>,
    /// Canonical chat id (empty when the platform has none).
    pub chat_id: String,
    /// Chat kind.
    pub chat_type: ChatType,
    /// Thread id, when inside one.
    pub thread_id: Option<String>,
    /// Canonical sender id.
    pub sender_id: String,
    /// Linked person, when linked.
    pub person_id: Option<String>,
}

/// Which conversations are per participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Isolation {
    /// Members of a non-thread group or channel each get a session.
    pub group_per_user: bool,
    /// Participants of a thread each get a session.
    pub thread_per_user: bool,
}

impl Default for Isolation {
    fn default() -> Self {
        Self {
            group_per_user: true,
            thread_per_user: false,
        }
    }
}

/// A conversation key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionKey(String);

impl SessionKey {
    /// The key text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The fields of a parsed key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyParts {
    /// Profile name.
    pub profile: String,
    /// Platform name.
    pub platform: String,
    /// Scope id.
    pub scope_id: Option<String>,
    /// Chat kind.
    pub chat_type: ChatType,
    /// Chat id.
    pub chat_id: String,
    /// Thread id.
    pub thread_id: Option<String>,
    /// Participant id.
    pub participant: Option<String>,
}

/// Percent-encodes `%` and `:` in an id.
#[must_use]
pub fn escape(id: &str) -> String {
    id.replace('%', "%25").replace(':', "%3A")
}

/// Reverses [`escape`].
#[must_use]
pub fn unescape(text: &str) -> String {
    text.replace("%3A", ":")
        .replace("%3a", ":")
        .replace("%25", "%")
}

impl Routing {
    /// The conversation key of this event under `isolation`: DMs per chat
    /// (the sender when the platform has no chat id), group and channel
    /// members apart unless `group_per_user` is off, threads shared unless
    /// `thread_per_user` is on.
    #[must_use]
    pub fn key(&self, isolation: Isolation) -> SessionKey {
        let mut key = format!("{}:{}", self.profile, escape(&self.platform));
        if let Some(scope) = &self.scope_id {
            key.push('@');
            key.push_str(&escape(scope));
        }
        let chat = if self.chat_id.is_empty() {
            &self.sender_id
        } else {
            &self.chat_id
        };
        key.push(':');
        key.push_str(self.chat_type.as_str());
        key.push(':');
        key.push_str(&escape(chat));
        if let Some(thread) = &self.thread_id {
            key.push_str(":t:");
            key.push_str(&escape(thread));
        }
        let per_user = match (self.chat_type, &self.thread_id) {
            (ChatType::Dm, _) => false,
            (ChatType::Thread, _) | (_, Some(_)) => isolation.thread_per_user,
            (ChatType::Group | ChatType::Channel, None) => isolation.group_per_user,
        };
        if per_user {
            key.push_str(":u:");
            key.push_str(&escape(&self.sender_id));
        }
        SessionKey(key)
    }
}

impl KeyParts {
    /// Parses a key; `None` when it is not one.
    #[must_use]
    pub fn parse(key: &str) -> Option<Self> {
        let mut segments = key.split(':');
        let profile = segments.next()?.to_owned();
        let platform_segment = segments.next()?;
        let (platform, scope_id) = match platform_segment.split_once('@') {
            Some((platform, scope)) => (unescape(platform), Some(unescape(scope))),
            None => (unescape(platform_segment), None),
        };
        let chat_type = ChatType::parse(segments.next()?)?;
        let chat_id = unescape(segments.next()?);
        let mut parts = Self {
            profile,
            platform,
            scope_id,
            chat_type,
            chat_id,
            thread_id: None,
            participant: None,
        };
        while let Some(tag) = segments.next() {
            let value = unescape(segments.next()?);
            match tag {
                "t" if parts.thread_id.is_none() && parts.participant.is_none() => {
                    parts.thread_id = Some(value);
                }
                "u" if parts.participant.is_none() => parts.participant = Some(value),
                _ => return None,
            }
        }
        Some(parts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routing(profile: &str, platform: &str, chat_type: ChatType, chat: &str) -> Routing {
        Routing {
            adapter: platform.to_owned(),
            profile: profile.to_owned(),
            platform: platform.to_owned(),
            scope_id: None,
            chat_id: chat.to_owned(),
            chat_type,
            thread_id: None,
            sender_id: "A".to_owned(),
            person_id: None,
        }
    }

    struct Case {
        name: &'static str,
        routing: Routing,
        isolation: Isolation,
        key: &'static str,
    }

    #[allow(clippy::too_many_lines)] // one table
    fn cases() -> Vec<Case> {
        let thread = |sender: &str, per_user: bool| Case {
            name: "thread",
            routing: Routing {
                thread_id: Some("1700.1".into()),
                sender_id: sender.into(),
                ..routing("default", "slack", ChatType::Thread, "C1")
            },
            isolation: Isolation {
                thread_per_user: per_user,
                ..Isolation::default()
            },
            key: if per_user {
                "default:slack:thread:C1:t:1700.1:u:B"
            } else {
                "default:slack:thread:C1:t:1700.1"
            },
        };
        vec![
            Case {
                name: "default profile DM",
                routing: routing("default", "telegram", ChatType::Dm, "123"),
                isolation: Isolation::default(),
                key: "default:telegram:dm:123",
            },
            Case {
                name: "profile main never aliases default",
                routing: routing("main", "telegram", ChatType::Dm, "123"),
                isolation: Isolation::default(),
                key: "main:telegram:dm:123",
            },
            Case {
                name: "DM without chat id falls back to the sender",
                routing: routing("default", "signal", ChatType::Dm, ""),
                isolation: Isolation::default(),
                key: "default:signal:dm:A",
            },
            Case {
                name: "Matrix room id is escaped",
                routing: routing("default", "matrix", ChatType::Group, "!abc:example.org"),
                isolation: Isolation {
                    group_per_user: false,
                    ..Isolation::default()
                },
                key: "default:matrix:group:!abc%3Aexample.org",
            },
            Case {
                name: "group member A",
                routing: routing("default", "discord", ChatType::Channel, "555"),
                isolation: Isolation::default(),
                key: "default:discord:channel:555:u:A",
            },
            Case {
                name: "group member B",
                routing: Routing {
                    sender_id: "B".into(),
                    ..routing("default", "discord", ChatType::Channel, "555")
                },
                isolation: Isolation::default(),
                key: "default:discord:channel:555:u:B",
            },
            Case {
                name: "shared group",
                routing: routing("default", "discord", ChatType::Group, "555"),
                isolation: Isolation {
                    group_per_user: false,
                    thread_per_user: false,
                },
                key: "default:discord:group:555",
            },
            Case {
                name: "scope id and a percent in an id",
                routing: Routing {
                    scope_id: Some("T01".into()),
                    ..routing("work", "slack", ChatType::Dm, "50%:off")
                },
                isolation: Isolation::default(),
                key: "work:slack@T01:dm:50%25%3Aoff",
            },
            thread("A", false),
            thread("B", false),
            thread("B", true),
        ]
    }

    #[test]
    fn session_keys_follow_the_grammar() {
        for case in cases() {
            let key = case.routing.key(case.isolation);
            assert_eq!(key.as_str(), case.key, "{}", case.name);
        }
    }

    #[test]
    fn every_key_parses_back_to_its_fields() {
        for case in cases() {
            let key = case.routing.key(case.isolation);
            let parts = KeyParts::parse(key.as_str()).unwrap_or_else(|| panic!("{}", case.name));
            let r = &case.routing;
            assert_eq!(parts.profile, r.profile, "{}", case.name);
            assert_eq!(parts.platform, r.platform, "{}", case.name);
            assert_eq!(parts.scope_id, r.scope_id, "{}", case.name);
            assert_eq!(parts.chat_type, r.chat_type, "{}", case.name);
            let chat = if r.chat_id.is_empty() {
                &r.sender_id
            } else {
                &r.chat_id
            };
            assert_eq!(&parts.chat_id, chat, "{}", case.name);
            assert_eq!(parts.thread_id, r.thread_id, "{}", case.name);
        }
        assert_eq!(
            KeyParts::parse("default:matrix:group:!abc%3Aexample.org")
                .unwrap()
                .chat_id,
            "!abc:example.org"
        );
    }

    #[test]
    fn malformed_keys_do_not_parse() {
        for bad in [
            "",
            "default",
            "default:telegram",
            "default:telegram:room:1",
            "a:b:dm:1:x:2",
        ] {
            assert_eq!(KeyParts::parse(bad), None, "{bad}");
        }
    }
}

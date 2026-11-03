//! Gateway keys for limits and destinations (spec: messaging-gateway,
//! Per-user rate limiting; Per-chat policies; Home channel and /sethome):
//! `[gateway.rate_limit]`, `[[gateway.chat_policies]]` and a platform's
//! home channel.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::gateway::PlatformBusy;

/// `[gateway.platforms.<name>.home_channel]`: the default destination of
/// scheduled output, platform-only send targets and notices without an
/// originating chat.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HomeChannel {
    /// The chat id.
    pub chat_id: String,
    /// The thread (forum topic) inside the chat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// A display name (also accepted as `platform:#name` in send targets).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl HomeChannel {
    /// How confirmations name it: the display name, else
    /// `chat_id[:thread_id]`.
    #[must_use]
    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.chat_target())
    }

    /// `chat_id[:thread_id]`, as a send target writes it after the
    /// platform.
    #[must_use]
    pub fn chat_target(&self) -> String {
        match &self.thread_id {
            Some(thread) => format!("{}:{thread}", self.chat_id),
            None => self.chat_id.clone(),
        }
    }
}

/// `[gateway.rate_limit]`: per-sender limits (admins and local clients are
/// exempt).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Messages a sender may send per minute (sustained rate).
    #[schemars(range(min = 1, max = 10_000))]
    pub messages_per_minute: u32,
    /// Messages a sender may send at once above the sustained rate.
    #[schemars(range(min = 1, max = 10_000))]
    pub burst: u32,
    /// Turns a sender may start per hour.
    #[schemars(range(min = 1, max = 100_000))]
    pub turns_per_hour: u32,
    /// USD a sender's turns may cost per local day (from the usage
    /// ledger); unset is no cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0))]
    pub daily_cost_usd: Option<f64>,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            messages_per_minute: 20,
            burst: 10,
            turns_per_hour: 120,
            daily_cost_usd: None,
        }
    }
}

/// Overrides of `[gateway.rate_limit]` (per platform or chat policy); unset
/// keys keep the shared value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RateLimitOverride {
    /// Overrides `messages_per_minute`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 10_000))]
    pub messages_per_minute: Option<u32>,
    /// Overrides `burst`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 10_000))]
    pub burst: Option<u32>,
    /// Overrides `turns_per_hour`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 100_000))]
    pub turns_per_hour: Option<u32>,
    /// Overrides `daily_cost_usd`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0))]
    pub daily_cost_usd: Option<f64>,
}

impl RateLimitConfig {
    /// `self` with `over`'s keys.
    #[must_use]
    pub fn with(&self, over: &RateLimitOverride) -> Self {
        Self {
            messages_per_minute: over.messages_per_minute.unwrap_or(self.messages_per_minute),
            burst: over.burst.unwrap_or(self.burst),
            turns_per_hour: over.turns_per_hour.unwrap_or(self.turns_per_hour),
            daily_cost_usd: over.daily_cost_usd.or(self.daily_cost_usd),
        }
    }
}

/// The chat kinds a policy can match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyChatType {
    /// Direct messages.
    Dm,
    /// Group chats.
    Group,
    /// Channels.
    Channel,
    /// Threads inside a group or channel.
    Thread,
}

impl PolicyChatType {
    /// The gateway's name of the kind.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dm => "dm",
            Self::Group => "group",
            Self::Channel => "channel",
            Self::Thread => "thread",
        }
    }
}

/// One `[[gateway.chat_policies]]` entry: what sessions of the chats it
/// matches get. A policy is resolved once when a session is created and
/// frozen for that session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ChatPolicy {
    /// The platform it applies to.
    pub platform: String,
    /// Matches this chat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    /// Matches this thread or forum topic of the chat (more specific than
    /// the chat alone; a chat policy also applies to its threads).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Matches this kind of chat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_type: Option<PolicyChatType>,
    /// Matches this sender.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// The model of matched sessions (`[<provider>/]<model>` or an alias).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Toolsets of matched sessions; only narrows the profile's enabled
    /// toolsets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolsets: Option<Vec<String>>,
    /// Skills preloaded into matched sessions; only enabled skills.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    /// Skills whose content opens each matched session as its first
    /// user-side context (never in the system prompt); enabled skills
    /// only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preload_skills: Option<Vec<String>>,
    /// Text appended to the platform hint of matched sessions' system
    /// prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_append: Option<String>,
    /// Busy handling in matched chats (over the platform's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy: Option<PlatformBusy>,
    /// Rate limits in matched chats (over the platform's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitOverride>,
    /// USD one turn of a matched session may cost.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0))]
    pub max_cost_usd_per_turn: Option<f64>,
}

/// What an inbound event is, for policy matching.
#[derive(Debug, Clone, Copy)]
pub struct PolicyFacts<'a> {
    /// Platform name.
    pub platform: &'a str,
    /// Chat id.
    pub chat_id: &'a str,
    /// Chat kind (`dm`, `group`, `channel`, `thread`).
    pub chat_type: &'a str,
    /// Canonical sender id.
    pub user_id: &'a str,
    /// Thread or forum topic id.
    pub thread_id: Option<&'a str>,
}

impl ChatPolicy {
    /// How specific the policy is: a chat outranks a sender, which
    /// outranks a chat kind; two policies of one platform with the same
    /// matchers are a tie (refused by validation).
    #[must_use]
    pub fn specificity(&self) -> u8 {
        u8::from(self.thread_id.is_some()) * 8
            + u8::from(self.chat_id.is_some()) * 4
            + u8::from(self.user_id.is_some()) * 2
            + u8::from(self.chat_type.is_some())
    }

    /// Whether the policy applies to `facts`.
    #[must_use]
    pub fn matches(&self, facts: &PolicyFacts<'_>) -> bool {
        self.platform == facts.platform
            && self.chat_id.as_deref().is_none_or(|c| c == facts.chat_id)
            && self
                .thread_id
                .as_deref()
                .is_none_or(|t| Some(t) == facts.thread_id)
            && self.user_id.as_deref().is_none_or(|u| u == facts.user_id)
            && self.chat_type.is_none_or(|t| t.as_str() == facts.chat_type)
    }

    /// The matchers, for comparing two policies.
    fn matchers(
        &self,
    ) -> (
        &str,
        Option<&str>,
        Option<&str>,
        Option<&str>,
        Option<PolicyChatType>,
    ) {
        (
            &self.platform,
            self.chat_id.as_deref(),
            self.thread_id.as_deref(),
            self.user_id.as_deref(),
            self.chat_type,
        )
    }

    /// Whether `self` and `other` match the same events with the same
    /// specificity (neither could win).
    #[must_use]
    pub fn ties_with(&self, other: &Self) -> bool {
        self.matchers() == other.matchers()
    }
}

/// The most specific policy of `policies` matching `facts`, with its index.
#[must_use]
pub fn resolve_policy<'a>(
    policies: &'a [ChatPolicy],
    facts: &PolicyFacts<'_>,
) -> Option<(usize, &'a ChatPolicy)> {
    policies
        .iter()
        .enumerate()
        .filter(|(_, p)| p.matches(facts))
        .max_by_key(|(index, p)| (p.specificity(), std::cmp::Reverse(*index)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(chat: Option<&str>, user: Option<&str>, kind: Option<PolicyChatType>) -> ChatPolicy {
        ChatPolicy {
            platform: "discord".into(),
            chat_id: chat.map(Into::into),
            user_id: user.map(Into::into),
            chat_type: kind,
            ..ChatPolicy::default()
        }
    }

    #[test]
    fn the_most_specific_match_wins() {
        let policies = vec![
            policy(None, None, Some(PolicyChatType::Group)),
            policy(Some("555"), None, None),
            policy(None, Some("u1"), None),
        ];
        let facts = |chat, user| PolicyFacts {
            platform: "discord",
            chat_id: chat,
            chat_type: "group",
            user_id: user,
            thread_id: None,
        };
        assert_eq!(resolve_policy(&policies, &facts("555", "u1")).unwrap().0, 1);
        assert_eq!(resolve_policy(&policies, &facts("9", "u1")).unwrap().0, 2);
        assert_eq!(resolve_policy(&policies, &facts("9", "u2")).unwrap().0, 0);
        let dm = PolicyFacts {
            chat_type: "dm",
            ..facts("9", "u2")
        };
        assert!(resolve_policy(&policies, &dm).is_none());
    }

    #[test]
    fn a_thread_policy_outranks_its_channels_and_the_channel_covers_other_threads() {
        let mut topic = policy(Some("-100555"), None, None);
        topic.thread_id = Some("42".into());
        let policies = vec![policy(Some("-100555"), None, None), topic];
        let facts = |thread| PolicyFacts {
            platform: "discord",
            chat_id: "-100555",
            chat_type: "thread",
            user_id: "u1",
            thread_id: thread,
        };
        assert_eq!(resolve_policy(&policies, &facts(Some("42"))).unwrap().0, 1);
        assert_eq!(resolve_policy(&policies, &facts(Some("7"))).unwrap().0, 0);
        assert_eq!(resolve_policy(&policies, &facts(None)).unwrap().0, 0);
    }

    #[test]
    fn same_matchers_tie() {
        assert!(policy(Some("1"), None, None).ties_with(&policy(Some("1"), None, None)));
        assert!(!policy(Some("1"), None, None).ties_with(&policy(Some("2"), None, None)));
        assert!(!policy(Some("1"), None, None).ties_with(&policy(None, Some("1"), None)));
    }

    #[test]
    fn an_override_replaces_only_its_keys() {
        let shared = RateLimitConfig::default();
        let over = RateLimitOverride {
            burst: Some(3),
            daily_cost_usd: Some(2.0),
            ..RateLimitOverride::default()
        };
        let merged = shared.with(&over);
        assert_eq!(merged.burst, 3);
        assert_eq!(merged.messages_per_minute, 20);
        assert_eq!(merged.daily_cost_usd, Some(2.0));
    }
}

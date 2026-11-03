//! Shared-credential profile routes (spec: messaging-gateway,
//! Shared-credential profile routes): a profile whose adapter holds a
//! platform credential routes matching inbound events to another served
//! profile, which accepts them only when its own
//! `gateway.accept_routes_from` names the receiving profile.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One `[[gateway.profile_routes]]` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRoute {
    /// The route's name (logs, `gateway status`).
    pub name: String,
    /// The platform whose events it routes.
    pub platform: String,
    /// Matches this workspace or server (Discord guild, Slack team).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guild_id: Option<String>,
    /// Matches this chat or channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    /// Matches this thread or forum topic (a channel route also matches
    /// its threads).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Matches this sender.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// The served profile matching events go to.
    pub profile: String,
}

/// What an inbound event offers to route matching.
#[derive(Debug, Clone, Copy)]
pub struct RouteFacts<'a> {
    /// Platform name.
    pub platform: &'a str,
    /// Workspace or server.
    pub guild_id: Option<&'a str>,
    /// Chat id.
    pub chat_id: &'a str,
    /// Thread id.
    pub thread_id: Option<&'a str>,
    /// Sender id.
    pub user_id: &'a str,
}

impl ProfileRoute {
    /// Additive specificity: `user_id` 16, `thread_id` 8, `chat_id` 4,
    /// `guild_id` 2.
    #[must_use]
    pub fn specificity(&self) -> u32 {
        [
            (self.user_id.is_some(), 16),
            (self.thread_id.is_some(), 8),
            (self.chat_id.is_some(), 4),
            (self.guild_id.is_some(), 2),
        ]
        .iter()
        .filter(|(set, _)| *set)
        .map(|(_, weight)| weight)
        .sum()
    }

    /// Whether every declared field holds for `facts`.
    #[must_use]
    pub fn matches(&self, facts: &RouteFacts<'_>) -> bool {
        let holds = |want: Option<&String>, have: Option<&str>| {
            want.is_none_or(|w| have == Some(w.as_str()))
        };
        self.platform == facts.platform
            && holds(self.guild_id.as_ref(), facts.guild_id)
            && holds(self.chat_id.as_ref(), Some(facts.chat_id))
            && holds(self.thread_id.as_ref(), facts.thread_id)
            && holds(self.user_id.as_ref(), Some(facts.user_id))
    }
}

/// The most specific route of `routes` matching `facts` (declaration
/// order breaks ties).
#[must_use]
pub fn resolve_route<'a>(
    routes: &'a [ProfileRoute],
    facts: &RouteFacts<'_>,
) -> Option<&'a ProfileRoute> {
    routes
        .iter()
        .enumerate()
        .filter(|(_, r)| r.matches(facts))
        .max_by_key(|(index, r)| (r.specificity(), std::cmp::Reverse(*index)))
        .map(|(_, r)| r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(
        name: &str,
        guild: Option<&str>,
        chat: Option<&str>,
        user: Option<&str>,
    ) -> ProfileRoute {
        ProfileRoute {
            name: name.into(),
            platform: "discord".into(),
            guild_id: guild.map(Into::into),
            chat_id: chat.map(Into::into),
            user_id: user.map(Into::into),
            profile: name.into(),
            ..ProfileRoute::default()
        }
    }

    #[test]
    fn the_most_specific_route_wins_and_a_channel_route_covers_its_threads() {
        let routes = vec![
            route("guild", Some("123"), None, None),
            route("channel", Some("123"), Some("c1"), None),
            route("person", None, None, Some("u9")),
            route("tie", Some("123"), None, None),
        ];
        let facts = |chat, thread, user| RouteFacts {
            platform: "discord",
            guild_id: Some("123"),
            chat_id: chat,
            thread_id: thread,
            user_id: user,
        };
        let pick = |f| resolve_route(&routes, &f).map(|r| r.name.as_str());
        assert_eq!(
            pick(facts("c2", None, "u1")),
            Some("guild"),
            "ties keep declaration order"
        );
        assert_eq!(pick(facts("c1", Some("t1"), "u1")), Some("channel"));
        assert_eq!(pick(facts("c1", None, "u9")), Some("person"));
        let elsewhere = RouteFacts {
            guild_id: Some("999"),
            ..facts("c3", None, "u1")
        };
        assert_eq!(pick(elsewhere), None, "unmatched events stay");
    }
}

//! `[gateway.groups]` and `[gateway.bots]` (spec: messaging-gateway, Group
//! mention gating and observe mode; Bot-authored message admission; Channel
//! history backfill), each with per-platform overrides under
//! `platforms.<name>` (a platform value wins, an absent one inherits).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Which bot-authored messages are admitted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AllowBots {
    /// None (default).
    #[default]
    None,
    /// Those that mention this bot.
    Mentions,
    /// All of them.
    All,
}

/// Defines the keys of a section once, for the section and its
/// per-platform override tables.
macro_rules! overridable {
    ($section:ident, $table:ident, $doc:literal,
     $($(#[$attr:meta])* $field:ident: $ty:ty,)*) => {
        /// One platform's overrides; each unset key inherits.
        #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        #[serde(default, deny_unknown_fields)]
        pub struct $table {
            $($(#[$attr])*
            #[serde(skip_serializing_if = "Option::is_none")]
            pub $field: Option<$ty>,)*
        }

        #[doc = $doc]
        #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        #[serde(default, deny_unknown_fields)]
        pub struct $section {
            $($(#[$attr])*
            #[serde(skip_serializing_if = "Option::is_none")]
            pub $field: Option<$ty>,)*
            /// Per-platform overrides, by platform name.
            #[serde(skip_serializing_if = "BTreeMap::is_empty")]
            pub platforms: BTreeMap<String, $table>,
        }

        impl $section {
            /// The overrides of `platform` over the section's own keys.
            fn merged(&self, platform: &str) -> $table {
                let own = self.platforms.get(platform);
                $table {
                    $($field: own
                        .and_then(|o| o.$field.clone())
                        .or_else(|| self.$field.clone()),)*
                }
            }
        }
    };
}

overridable! {
    GatewayGroups, PlatformGroups, "`[gateway.groups]`: when group, channel and thread messages start a turn.",
    /// Only messages that mention the bot, reply to it, match a
    /// `mention_patterns` entry or are in a thread it takes part in start a
    /// turn (default true).
    require_mention: bool,
    /// Case-insensitive regular expressions that count as a mention (at
    /// most 32).
    mention_patterns: Vec<String>,
    /// A thread the bot takes part in still needs a mention (default
    /// false).
    thread_require_mention: bool,
    /// Chats answered without a mention.
    free_response_chats: Vec<String>,
    /// Chats and threads that never start a turn.
    ignored_chats: Vec<String>,
    /// Keep unmentioned messages of authorized group chats in the chat's
    /// session as attributed context, without a turn (default false).
    observe_unmentioned: bool,
    /// Give a mention-triggered turn the channel's messages since the
    /// bot's last reply (default true on Discord and Slack).
    history_backfill: bool,
    /// The most channel messages backfilled (default 50).
    #[schemars(range(min = 1, max = 200))]
    history_backfill_limit: u32,
}

overridable! {
    GatewayBots, PlatformBots, "`[gateway.bots]`: messages other bots send.",
    /// `none` (default), `mentions` or `all`.
    allow_bots: AllowBots,
    /// An admitted bot message starts a turn only with a literal inline
    /// mention of this bot; a reply ping alone does not (default true).
    require_inline_mention: bool,
    /// Follow-up chunks of the same bot in the same chat within this many
    /// milliseconds join the same turn (default 1500).
    #[schemars(range(max = 60_000))]
    burst_window_ms: u64,
    /// Senders treated as people although their messages are marked as a
    /// bot's (posts made with the owner's own user token).
    human_user_ids: Vec<String>,
}

/// Group gating in effect on one platform.
#[allow(clippy::struct_excessive_bools)] // independent gating switches
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveGroups {
    /// See [`GatewayGroups::require_mention`].
    pub require_mention: bool,
    /// See [`GatewayGroups::mention_patterns`].
    pub mention_patterns: Vec<String>,
    /// See [`GatewayGroups::thread_require_mention`].
    pub thread_require_mention: bool,
    /// See [`GatewayGroups::free_response_chats`].
    pub free_response_chats: Vec<String>,
    /// See [`GatewayGroups::ignored_chats`].
    pub ignored_chats: Vec<String>,
    /// See [`GatewayGroups::observe_unmentioned`].
    pub observe_unmentioned: bool,
    /// See [`GatewayGroups::history_backfill`].
    pub history_backfill: bool,
    /// See [`GatewayGroups::history_backfill_limit`].
    pub history_backfill_limit: usize,
}

impl GatewayGroups {
    /// The gating in effect on `platform`.
    #[must_use]
    pub fn for_platform(&self, platform: &str) -> EffectiveGroups {
        let merged = self.merged(platform);
        EffectiveGroups {
            require_mention: merged.require_mention.unwrap_or(true),
            mention_patterns: merged.mention_patterns.unwrap_or_default(),
            thread_require_mention: merged.thread_require_mention.unwrap_or(false),
            free_response_chats: merged.free_response_chats.unwrap_or_default(),
            ignored_chats: merged.ignored_chats.unwrap_or_default(),
            observe_unmentioned: merged.observe_unmentioned.unwrap_or(false),
            history_backfill: merged
                .history_backfill
                .unwrap_or(matches!(platform, "discord" | "slack")),
            history_backfill_limit: merged.history_backfill_limit.unwrap_or(50) as usize,
        }
    }
}

/// Bot admission in effect on one platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveBots {
    /// See [`GatewayBots::allow_bots`].
    pub allow_bots: AllowBots,
    /// See [`GatewayBots::require_inline_mention`].
    pub require_inline_mention: bool,
    /// See [`GatewayBots::burst_window_ms`].
    pub burst_window_ms: u64,
    /// See [`GatewayBots::human_user_ids`].
    pub human_user_ids: Vec<String>,
}

impl GatewayBots {
    /// The admission in effect on `platform`.
    #[must_use]
    pub fn for_platform(&self, platform: &str) -> EffectiveBots {
        let merged = self.merged(platform);
        EffectiveBots {
            allow_bots: merged.allow_bots.unwrap_or_default(),
            require_inline_mention: merged.require_inline_mention.unwrap_or(true),
            burst_window_ms: merged.burst_window_ms.unwrap_or(1500),
            human_user_ids: merged.human_user_ids.unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_overrides_win_and_defaults_fill_the_rest() {
        let groups: GatewayGroups = toml::from_str(
            "observe_unmentioned = true\n[platforms.telegram]\nrequire_mention = false\n",
        )
        .unwrap();
        let telegram = groups.for_platform("telegram");
        assert!(!telegram.require_mention && telegram.observe_unmentioned);
        assert!(!telegram.history_backfill);
        let discord = groups.for_platform("discord");
        assert!(discord.require_mention && discord.history_backfill);
        assert_eq!(discord.history_backfill_limit, 50);
        let bots = GatewayBots::default().for_platform("discord");
        assert_eq!(bots.allow_bots, AllowBots::None);
        assert!(bots.require_inline_mention);
        assert!(toml::from_str::<GatewayBots>("allow = \"all\"\n").is_err());
    }
}

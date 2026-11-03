//! `[gateway.display]` (spec: messaging-gateway, Display policy and
//! per-platform overrides; Runtime metadata footer; Warning notification
//! suppression) and `[gateway.message_timestamps]` (Message timestamps in
//! model context). Every display key can be overridden for one platform
//! under `[gateway.display.platforms.<name>]`: a platform value wins, an
//! absent one inherits, an unknown key fails validation.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::interface::ToolProgressGrouping;

/// How tool calls show in a chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChatToolProgress {
    /// Nothing.
    Off,
    /// A line when the tool changes.
    New,
    /// A line per tool call.
    All,
    /// Every call with a longer preview, and each result.
    Verbose,
    /// Nothing in the chat; each call is appended to
    /// `<home>/logs/<profile>/tool_calls.log`.
    Log,
}

impl ChatToolProgress {
    /// The next mode `/verbose` cycles to.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::New,
            Self::New => Self::All,
            Self::All => Self::Verbose,
            Self::Verbose | Self::Log => Self::Off,
        }
    }

    /// The configured name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::New => "new",
            Self::All => "all",
            Self::Verbose => "verbose",
            Self::Log => "log",
        }
    }
}

/// How a status phrase file combines with the built-in phrases.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PhraseMode {
    /// After the built-in phrases.
    #[default]
    Append,
    /// Instead of them.
    Replace,
}

/// `status_phrases`: the working-status phrases a surface rotates through
/// (never interpolating tool arguments).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusPhrases {
    /// A file under the profile directory, one phrase per line (at most
    /// 80 phrases of 160 characters).
    pub path: String,
    /// `append` (default) or `replace`.
    #[serde(default)]
    pub mode: PhraseMode,
}

/// Defines the display keys once for both tables: `[gateway.display]`
/// (with its `platforms` overrides) and each override table.
macro_rules! display_keys {
    ($($(#[$attr:meta])* $field:ident: $ty:ty,)*) => {
        /// One platform's display overrides; each unset key inherits from
        /// `[gateway.display]`, then the built-in default.
        #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        #[serde(default, deny_unknown_fields)]
        pub struct PlatformDisplay {
            $($(#[$attr])*
            #[serde(skip_serializing_if = "Option::is_none")]
            pub $field: Option<$ty>,)*
        }

        /// `[gateway.display]`.
        #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        #[serde(default, deny_unknown_fields)]
        pub struct GatewayDisplay {
            $($(#[$attr])*
            #[serde(skip_serializing_if = "Option::is_none")]
            pub $field: Option<$ty>,)*
            /// Per-platform overrides, by platform name.
            #[serde(skip_serializing_if = "BTreeMap::is_empty")]
            pub platforms: BTreeMap<String, PlatformDisplay>,
        }

        impl GatewayDisplay {
            /// The keys set for every platform.
            fn base(&self) -> PlatformDisplay {
                PlatformDisplay { $($field: self.$field.clone(),)* }
            }
        }
    };
}

display_keys! {
    /// `off`, `new`, `all` (default; `off` on Telegram), `verbose` or
    /// `log`.
    tool_progress: ChatToolProgress,
    /// `accumulate` (default: one edited bubble) or `separate`.
    tool_progress_grouping: ToolProgressGrouping,
    /// Characters of a tool call's arguments shown (default 40).
    #[schemars(range(max = 2000))]
    tool_preview_length: u32,
    /// Send what the model says between tool calls (default true).
    interim_assistant_messages: bool,
    /// One edited `Working — <n> min` bubble every 180 seconds of a long
    /// turn (default true).
    long_running_notifications: bool,
    /// Busy acknowledgements name what the running turn is doing (default
    /// false).
    busy_ack_detail: bool,
    /// Delete the progress bubbles after a successful final reply on
    /// adapters that can delete (default false).
    cleanup_progress: bool,
    /// `/verbose` cycles the chat's tool progress mode (default false).
    tool_progress_command: bool,
    /// Working-status phrases.
    status_phrases: StatusPhrases,
    /// Append a model / context / directory footer to final replies
    /// (default false; `/footer on|off` per chat).
    runtime_footer: bool,
    /// Mute automatic warning and error notices on this destination
    /// (default false; logs and stored state are unchanged).
    suppress_warning_notifications: bool,
}

/// The display settings in effect on one platform.
#[allow(clippy::struct_excessive_bools)] // independent display switches
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveDisplay {
    /// See [`PlatformDisplay::tool_progress`].
    pub tool_progress: ChatToolProgress,
    /// See [`PlatformDisplay::tool_progress_grouping`].
    pub grouping: ToolProgressGrouping,
    /// See [`PlatformDisplay::tool_preview_length`].
    pub preview_length: usize,
    /// See [`PlatformDisplay::interim_assistant_messages`].
    pub interim_messages: bool,
    /// See [`PlatformDisplay::long_running_notifications`].
    pub long_running: bool,
    /// See [`PlatformDisplay::busy_ack_detail`].
    pub busy_ack_detail: bool,
    /// See [`PlatformDisplay::cleanup_progress`].
    pub cleanup_progress: bool,
    /// See [`PlatformDisplay::tool_progress_command`].
    pub tool_progress_command: bool,
    /// See [`PlatformDisplay::status_phrases`].
    pub status_phrases: Option<StatusPhrases>,
    /// See [`PlatformDisplay::runtime_footer`].
    pub runtime_footer: bool,
    /// See [`PlatformDisplay::suppress_warning_notifications`].
    pub suppress_warnings: bool,
}

impl GatewayDisplay {
    /// The settings in effect on `platform`.
    #[must_use]
    pub fn for_platform(&self, platform: &str) -> EffectiveDisplay {
        let own = self.platforms.get(platform);
        let all = self.base();
        let pick =
            |get: fn(&PlatformDisplay) -> Option<bool>| own.and_then(get).or_else(|| get(&all));
        let progress_default = if platform == "telegram" {
            ChatToolProgress::Off
        } else {
            ChatToolProgress::All
        };
        EffectiveDisplay {
            tool_progress: own
                .and_then(|o| o.tool_progress)
                .or(all.tool_progress)
                .unwrap_or(progress_default),
            grouping: own
                .and_then(|o| o.tool_progress_grouping)
                .or(all.tool_progress_grouping)
                .unwrap_or(ToolProgressGrouping::Accumulate),
            preview_length: own
                .and_then(|o| o.tool_preview_length)
                .or(all.tool_preview_length)
                .unwrap_or(40) as usize,
            interim_messages: pick(|s| s.interim_assistant_messages).unwrap_or(true),
            long_running: pick(|s| s.long_running_notifications).unwrap_or(true),
            busy_ack_detail: pick(|s| s.busy_ack_detail).unwrap_or(false),
            cleanup_progress: pick(|s| s.cleanup_progress).unwrap_or(false),
            tool_progress_command: pick(|s| s.tool_progress_command).unwrap_or(false),
            status_phrases: own
                .and_then(|o| o.status_phrases.clone())
                .or_else(|| all.status_phrases.clone()),
            runtime_footer: pick(|s| s.runtime_footer).unwrap_or(false),
            suppress_warnings: pick(|s| s.suppress_warning_notifications).unwrap_or(false),
        }
    }
}

/// `[gateway.message_timestamps]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MessageTimestamps {
    /// Prefix each user message the model sees with its send time in the
    /// profile's time zone (`[Tue 2026-04-28 13:40:53 CEST]`). Frozen per
    /// session when it is created: a change applies to new sessions.
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_platform_value_wins_and_an_absent_one_inherits() {
        let config: GatewayDisplay = toml::from_str(
            "tool_progress = \"new\"\nsuppress_warning_notifications = true\n\
             [platforms.telegram]\nsuppress_warning_notifications = false\n",
        )
        .unwrap();
        let telegram = config.for_platform("telegram");
        assert_eq!(telegram.tool_progress, ChatToolProgress::New, "inherited");
        assert!(!telegram.suppress_warnings, "the platform wins");
        assert!(config.for_platform("slack").suppress_warnings);
    }

    #[test]
    fn telegram_shows_no_progress_by_default() {
        let config = GatewayDisplay::default();
        assert_eq!(
            config.for_platform("telegram").tool_progress,
            ChatToolProgress::Off
        );
        let discord = config.for_platform("discord");
        assert_eq!(discord.tool_progress, ChatToolProgress::All);
        assert_eq!(discord.grouping, ToolProgressGrouping::Accumulate);
        assert_eq!(discord.preview_length, 40);
    }

    #[test]
    fn an_unknown_key_fails() {
        assert!(
            toml::from_str::<GatewayDisplay>("[platforms.slack]\nshow_tools = true\n").is_err()
        );
        assert!(toml::from_str::<GatewayDisplay>("show_tools = true\n").is_err());
    }
}

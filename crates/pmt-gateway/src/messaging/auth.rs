//! Who may talk to the agent (spec: messaging-gateway, Authorization order
//! and fail-closed default; Slash command access tiers; platform-adapters,
//! Adapter access-policy delegation). The first rule that admits a sender
//! wins, in a fixed order, and anything no rule admits is denied; no
//! environment variable takes part. An adapter's own access policy is
//! trusted last, only in `allowlist` mode and only on an explicit `allow`
//! from its sender check: a `deny`, an `unknown`, a failure or a timeout
//! leave the default deny.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use pmt_config::schema::PlatformConfig;
use tokio::time::Instant;

use super::adapter::{AccessPolicy, Adapter, Descriptor, Inbound, SenderVerdict};
use super::key::ChatType;

/// The rule that admitted a sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// The connector authorizes upstream (the relay).
    Upstream,
    /// `allow_chats` lists the chat.
    Chat,
    /// `allow_all`.
    All,
    /// An approved pairing.
    Paired,
    /// `allow_users` lists the sender.
    User,
    /// The adapter's own allowlist.
    AdapterAllowlist,
}

/// Everything the order looks at.
#[derive(Debug, Clone, Copy)]
pub struct Facts<'a> {
    /// The adapter's descriptor.
    pub descriptor: &'a Descriptor,
    /// The platform table.
    pub config: &'a PlatformConfig,
    /// The event.
    pub inbound: &'a Inbound,
    /// The sender has an approved pairing.
    pub paired: bool,
    /// The adapter's sender check ([`adapter_verdict`]); `Unknown` when
    /// not asked.
    pub adapter: SenderVerdict,
}

/// How long an adapter's sender check may take before it counts as
/// `unknown`.
pub const SENDER_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether the adapter's access policy should be asked about the sender:
/// only an `allowlist` policy is ever trusted.
#[must_use]
pub fn asks_adapter(descriptor: &Descriptor) -> bool {
    descriptor.access_policy == Some(AccessPolicy::Allowlist)
}

/// The adapter's verdict on the event's sender; a failing or slow check
/// is `unknown`.
pub async fn adapter_verdict(adapter: &dyn Adapter, inbound: &Inbound) -> SenderVerdict {
    match tokio::time::timeout(SENDER_CHECK_TIMEOUT, adapter.authorize_sender(inbound)).await {
        Ok(Err(err)) => {
            tracing::warn!(
                code = err.code,
                "the adapter's sender check failed: {}",
                err.message
            );
            SenderVerdict::Unknown
        }
        Ok(answer) => SenderVerdict::of(answer),
        Err(_) => {
            tracing::warn!("the adapter's sender check timed out");
            SenderVerdict::Unknown
        }
    }
}

/// The rule admitting the sender, or `None` (deny).
#[must_use]
pub fn authorize(facts: &Facts<'_>) -> Option<Rule> {
    let Facts {
        descriptor,
        config,
        inbound,
        paired,
        adapter,
    } = *facts;
    if descriptor.authorization_is_upstream {
        Some(Rule::Upstream)
    } else if config.allow_chats.contains(&inbound.chat_id) {
        Some(Rule::Chat)
    } else if config.allow_all {
        Some(Rule::All)
    } else if paired {
        Some(Rule::Paired)
    } else if config.allow_users.contains(&inbound.sender_id) {
        Some(Rule::User)
    } else if asks_adapter(descriptor) && adapter == SenderVerdict::Allow {
        Some(Rule::AdapterAllowlist)
    } else {
        None
    }
}

/// Commands every authorized user may run where an admin list is set.
pub const USER_TIER: &[&str] = &["help", "whoami", "stop", "new", "queue", "link"];

/// Whether `sender` is an admin of the chat's scope (`admins` for DMs,
/// `group_admins` elsewhere); `None` when that scope has no admin list.
#[must_use]
pub fn is_admin(config: &PlatformConfig, chat_type: ChatType, sender: &str) -> Option<bool> {
    let admins = if chat_type == ChatType::Dm {
        &config.admins
    } else {
        &config.group_admins
    };
    (!admins.is_empty()).then(|| admins.iter().any(|a| a == sender))
}

/// Whether `sender` may run `command` in a chat of `chat_type`: with no
/// admin list for the scope everyone may, otherwise admins may run every
/// command and others the user tier plus `user_commands`.
#[must_use]
pub fn may_run(config: &PlatformConfig, chat_type: ChatType, sender: &str, command: &str) -> bool {
    match is_admin(config, chat_type, sender) {
        None | Some(true) => true,
        Some(false) => {
            USER_TIER.contains(&command) || config.user_commands.iter().any(|c| c == command)
        }
    }
}

/// The refusal for a command outside the sender's tier.
pub const ADMIN_ONLY: &str = "This command is restricted to admins.";

/// Bot-to-bot exchanges counted per chat within [`BOT_WINDOW`] before the
/// chat cools down.
pub const BOT_EXCHANGES: usize = 5;
/// The window bot messages are counted in, and the cool-down.
pub const BOT_WINDOW: Duration = Duration::from_mins(1);

/// Stops two bots from talking to each other forever: after
/// [`BOT_EXCHANGES`] bot-authored messages admitted in one chat within a
/// minute, that chat refuses bot messages for a minute.
#[derive(Debug, Default)]
pub struct BotLoopGuard {
    chats: Mutex<HashMap<String, Chat>>,
}

#[derive(Debug, Default)]
struct Chat {
    admitted: VecDeque<Instant>,
    cooling_until: Option<Instant>,
}

impl BotLoopGuard {
    /// Whether a bot message in `chat` may pass at `now` (it is counted
    /// when it does).
    ///
    /// # Panics
    ///
    /// When the guard's lock is poisoned.
    pub fn admit(&self, chat: &str, now: Instant) -> bool {
        let mut chats = self.chats.lock().expect("bot guard poisoned");
        let state = chats.entry(chat.to_owned()).or_default();
        if state.cooling_until.is_some_and(|until| now < until) {
            return false;
        }
        state.cooling_until = None;
        while state
            .admitted
            .front()
            .is_some_and(|at| now.duration_since(*at) >= BOT_WINDOW)
        {
            state.admitted.pop_front();
        }
        state.admitted.push_back(now);
        if state.admitted.len() >= BOT_EXCHANGES {
            state.admitted.clear();
            state.cooling_until = Some(now + BOT_WINDOW);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(
        descriptor: &'a Descriptor,
        config: &'a PlatformConfig,
        inbound: &'a Inbound,
    ) -> Facts<'a> {
        Facts {
            descriptor,
            config,
            inbound,
            paired: false,
            adapter: SenderVerdict::Unknown,
        }
    }

    #[test]
    fn nothing_configured_denies() {
        let descriptor = Descriptor::new("discord", 2000);
        let config = PlatformConfig::default();
        let inbound = Inbound::text(ChatType::Channel, "555", "stranger", "@bot hi");
        assert_eq!(authorize(&facts(&descriptor, &config, &inbound)), None);
    }

    #[test]
    fn the_first_matching_rule_wins_in_order() {
        let mut descriptor = Descriptor::new("discord", 2000);
        let mut config = PlatformConfig {
            allow_users: vec!["u".into()],
            ..PlatformConfig::default()
        };
        let inbound = Inbound::text(ChatType::Dm, "c", "u", "hi");
        assert_eq!(
            authorize(&facts(&descriptor, &config, &inbound)),
            Some(Rule::User)
        );
        let paired = Facts {
            paired: true,
            ..facts(&descriptor, &config, &inbound)
        };
        assert_eq!(authorize(&paired), Some(Rule::Paired));
        config.allow_all = true;
        assert_eq!(
            authorize(&facts(&descriptor, &config, &inbound)),
            Some(Rule::All)
        );
        config.allow_chats = vec!["c".into()];
        assert_eq!(
            authorize(&facts(&descriptor, &config, &inbound)),
            Some(Rule::Chat)
        );
        descriptor.authorization_is_upstream = true;
        assert_eq!(
            authorize(&facts(&descriptor, &config, &inbound)),
            Some(Rule::Upstream)
        );
    }

    #[test]
    fn only_an_allow_from_an_adapter_allowlist_is_trusted() {
        use SenderVerdict::{Allow, Deny, Unknown};
        let config = PlatformConfig::default();
        let inbound = Inbound::text(ChatType::Dm, "c", "u", "hi");
        let table = [
            (
                Some(AccessPolicy::Allowlist),
                Allow,
                Some(Rule::AdapterAllowlist),
            ),
            (Some(AccessPolicy::Allowlist), Deny, None),
            (Some(AccessPolicy::Allowlist), Unknown, None),
            (Some(AccessPolicy::Open), Allow, None),
            (Some(AccessPolicy::Open), Unknown, None),
            (None, Allow, None),
        ];
        for (policy, verdict, expected) in table {
            let descriptor = Descriptor {
                access_policy: policy,
                ..Descriptor::new("x", 100)
            };
            let facts = Facts {
                adapter: verdict,
                ..facts(&descriptor, &config, &inbound)
            };
            assert_eq!(authorize(&facts), expected, "{policy:?} {verdict:?}");
        }
    }

    #[test]
    fn answers_other_than_allow_or_deny_and_failures_are_unknown() {
        use SenderVerdict::{Allow, Deny, Unknown};
        let table = [
            ("allow", Allow),
            ("deny", Deny),
            ("unknown", Unknown),
            ("Allow", Unknown),
            ("yes", Unknown),
            ("", Unknown),
        ];
        for (answer, expected) in table {
            assert_eq!(SenderVerdict::from_answer(answer), expected, "{answer:?}");
        }
        assert_eq!(SenderVerdict::of::<()>(Ok(Allow)), Allow);
        assert_eq!(SenderVerdict::of(Err("plugin trapped")), Unknown);
    }

    #[test]
    fn a_non_admin_keeps_only_the_user_tier() {
        let config = PlatformConfig {
            admins: vec!["111".into()],
            user_commands: vec!["usage".into()],
            ..PlatformConfig::default()
        };
        assert!(!may_run(&config, ChatType::Dm, "222", "model"));
        assert!(may_run(&config, ChatType::Dm, "222", "help"));
        assert!(may_run(&config, ChatType::Dm, "222", "usage"));
        assert!(may_run(&config, ChatType::Dm, "111", "model"));
        assert!(
            may_run(&config, ChatType::Group, "222", "model"),
            "no group_admins: every command in groups"
        );
    }

    #[test]
    fn five_bot_exchanges_cool_the_chat_down_for_a_minute() {
        let guard = BotLoopGuard::default();
        let start = Instant::now();
        for i in 0..5 {
            assert!(guard.admit("c", start + Duration::from_secs(i)), "{i}");
        }
        assert!(!guard.admit("c", start + Duration::from_secs(10)));
        assert!(guard.admit("other", start + Duration::from_secs(10)));
        assert!(!guard.admit("c", start + Duration::from_secs(63)));
        assert!(guard.admit("c", start + Duration::from_secs(65)));
    }
}
